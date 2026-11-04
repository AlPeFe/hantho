# Han Thỏ como servidor MCP

[English](mcp.md) · **Español**

`hantho mcp` expone Han Thỏ por [MCP](https://modelcontextprotocol.io) (stdio). Así un agente
de terminal —Claude Code, Codex, Hermes, pi o cualquier cliente MCP— puede buscar manga, consultar
tu biblioteca, marcar capítulos y **abrir el lector en un panel al lado del chat**.

![Un agente de terminal abre Han Thỏ en un panel lateral](../assets/screenshots/mcp.png)

## Herramientas

| Herramienta | Qué hace |
|---|---|
| `search_manga` | Busca un título en las fuentes instaladas (con más de 20, en una primera tanda de las más fiables; `all_sources` busca en todas); por defecto, solo en los idiomas elegidos en la TUI |
| `library` | Tus favoritos con estado, progreso, último capítulo y vínculo con AniList |
| `chapters` | Capítulos de un manga, con cuáles están leídos (`from`, `to`, `unread_only`, `limit`) |
| `mark_read` | Marca o desmarca un capítulo, o todos hasta un número; actualiza AniList si está vinculado |
| `set_favorite` | Añade o quita un manga de favoritos; al añadirlo lo vincula con AniList si el título coincide exactamente |
| `open_reader` | Abre el lector en un panel lateral (o en una ventana nueva), por defecto donde te quedaste |
| `link_anilist` | Vincula un favorito con su obra de AniList y sincroniza; si hay dudas devuelve candidatas para elegir |
| `sync_anilist` | Envía a AniList el estado y el progreso de un favorito vinculado |

Ejemplos de lo que puedes pedirle al agente:

- «Ábreme Grand Blue por donde iba»
- «¿En qué capítulo voy de cada manga de mi biblioteca?»
- «Busca *Dungeon Meshi* en fuentes en español y añádelo a favoritos»
- «Marca como leídos los capítulos de Grand Blue hasta el 60»
- «Vincula Grand Blue con mi AniList»

> Tener la cuenta de AniList conectada no basta: cada manga se vincula una vez con su obra de
> AniList (el mismo título puede tener manga, novela y spin-offs). Al añadir un favorito se vincula
> solo si el título coincide exactamente; si no, `link_anilist` (o la tecla `a` en la TUI).

## Panel lateral

`open_reader` divide la ventana de la terminal en la que corre el agente y abre Han Thỏ a la
derecha:

| Terminal | Cómo |
|---|---|
| Windows Terminal | `wt -w 0 split-pane` (las imágenes Sixel funcionan por panel) |
| tmux | `tmux split-window -h` |
| WezTerm | `wezterm cli split-pane --right` |
| zellij | `zellij run --direction right` |
| kitty | `kitty @ launch --location=vsplit` (requiere `allow_remote_control yes`) |

Si la terminal no se puede dividir, se abre una ventana nueva (`placement: "window"` lo fuerza).
Lo mismo se puede hacer a mano: `hantho open "grand blue" --continue --side`.

La biblioteca y los capítulos leídos se comparten: si el agente marca algo mientras tienes la TUI
abierta, la TUI lo recarga sola en un par de segundos.

## Conectarlo

Usa la ruta completa al binario. En Windows, por ejemplo,
`C:\Users\<tú>\hantho\reader\target\release\hantho.exe`; en Linux,
`/ruta/al/repo/reader/target/release/hantho`.

### Claude Code

```bash
claude mcp add --scope user hantho -- /ruta/a/hantho mcp
```

### Codex CLI

En `~/.codex/config.toml`:

```toml
[mcp_servers.hantho]
command = "/ruta/a/hantho"
args = ["mcp"]
```

### Otros clientes (formato `mcpServers`)

```json
{
  "mcpServers": {
    "hantho": { "command": "/ruta/a/hantho", "args": ["mcp"] }
  }
}
```

Si Java no está en el `PATH` del agente, añade la variable `JAVA_HOME` en la configuración del
servidor (`"env": { "JAVA_HOME": "…" }`).

## Detalles

- JSON-RPC 2.0 por stdio, un mensaje por línea; versiones de protocolo `2025-06-18`,
  `2025-03-26` y `2024-11-05`.
- El host de extensiones (JVM) arranca con la primera herramienta que lo necesita y se reutiliza.
- Las búsquedas y los capítulos usan la misma caché que la TUI, y AniList el mismo limitador de
  pedidos.
- Los diagnósticos van a stderr y los errores del host a `~/.hantho/host.log`.
