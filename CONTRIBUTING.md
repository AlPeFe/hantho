# Cómo contribuir

¡Gracias por querer ayudar! Algunas pautas cortas:

## Antes de abrir un issue

- **¿Una fuente no funciona o falta alguna?** Si también falla en Mihon, el problema es de la extensión:
  repórtalo en [keiyoushi/extensions](https://github.com/keiyoushi/extensions/issues). Si funciona en
  Mihon pero no en Han Thỏ, es nuestro (probablemente falta un *shim* de Android): abre un issue
  con el nombre de la extensión y el error que muestra `hantho search --all`.
- Las fuentes con Cloudflare no se pueden usar por ahora (ver *Limitaciones* en el README).
- No compartas tu token de AniList ni el contenido de `~/.hantho/anilist.json` en un issue.

## Desarrollo

```bash
(cd ext-host && ./gradlew installDist)        # gradlew.bat en Windows
(cd reader && cargo build && cargo test)
```

- `reader/` (Rust): TUI, búsqueda, render, biblioteca, AniList.
- `ext-host/` (Kotlin): carga las extensiones y responde peticiones JSON por stdin/stdout.
  Probarlo a mano: `echo '{"id":1,"op":"ping"}' | ext-host/build/install/ext-host/bin/ext-host`.
- `tools/`: scripts de prueba en Python (cliente del host, pruebas de Sixel).
- `NOTES.md` tiene la investigación técnica (formato del índice, API de extensions-lib, render).

Usa `HANTHO_HOME=/tmp/hantho-dev` para no tocar tus datos reales mientras pruebas.

## Pull requests

- Uno por tema, pequeño y centrado. Si es un cambio grande, abre antes un issue para comentarlo.
- Tiene que pasar el CI (Linux y Windows): `cargo build`, `cargo test` y el build de `ext-host`.
- Ejecuta `cargo fmt` y `cargo clippy` antes de enviarlo.
- Para cambios en la TUI, añade una captura al PR.
- Para publicar binarios: sube una etiqueta `vX.Y.Z` (ver `.github/workflows/release.yml`).
- Comentarios y mensajes de commit en español, como el resto del código.

Al contribuir, aceptas que tu aporte se publique bajo la [licencia Apache-2.0](LICENSE) del proyecto.
