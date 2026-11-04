# Han Thỏ: notas de investigación

Lector de manga para terminal. Rust (motor, TUI, render) + `ext-host` en Kotlin/JVM
que ejecuta las extensiones de Mihon/Keiyoushi.

## Índice de extensiones
- `https://raw.githubusercontent.com/keiyoushi/extensions/repo/index.pb`: protobuf comprimido con gzip.
- `Repo { 1 name, 2 shortName, 3 signingKeySha256, 4 Links, 101 Index { repeated Extension 1 } }`
- `Extension { 1 name, 2 pkg, 3 Urls{1 apk, 2 icon, 501 jar}, 4 libVersion, 5 versionCode,
  6 versionName, 7 contentWarning (1 SAFE, 2 MIXED, 3 NSFW), 8 repeated Source{1 id, 2 name, 3 lang, 4 baseUrl} }`
- 1440 extensiones, 2353 fuentes, 150 idiomas. 694 extensiones usan uno de 67 temas multisrc (madara: 241, mangathemesia: 113).

## JARs de extensiones
- Son `.class` JVM estándar (ya convertidos de dex), minificados.
- Punto de entrada: `AndroidManifest.xml` en texto plano dentro del jar, `meta-data tachiyomi.extension.class`
  (normalmente `keiyoushi.source.Generated`). También están `tachiyomix.contentWarning` y `tachiyomix.extensionLib`.
- No traen `eu.kanade.tachiyomi.source.*` (source-api), OkHttp, Jsoup, kotlinx ni Injekt: el host los provee.
- Clases de Android referenciadas en una muestra de 7 jars (hay que hacer shims):
  - `android.app.{Activity, Application}`, `android.content.{Context, Intent, ActivityNotFoundException, SharedPreferences(+Editor)}`
  - `android.net.Uri`, `android.os.{Bundle, SystemClock, Build$VERSION}`, `android.util.{Log, Base64}`
  - `android.text.{Editable, TextWatcher}`, `android.view.View`, `android.widget.{Button, EditText}`, `android.preference.PreferenceManager`
  - `androidx.preference.*` (pantalla de ajustes: EditText/List/MultiSelect/Switch, PreferenceScreen)
  - Activity/Intent solo se usan en `UrlActivity` (deep links): no hace falta ejecutarlos, solo que las clases existan.

## API que implementa ext-host (keiyoushi/extensions-lib 1.6)
- La librería publicada son stubs (`throw Exception("Stub!")`); ext-host trae la implementación real con firmas idénticas.
- API moderna con suspend: `getSearchManga`, `getMangaUpdate(manga, chapters, fetchDetails, fetchChapters)`, `getPageList`, `getImageUrl`.
- `keiyoushi.source.KeiSource` (core empaquetado en cada jar) valida el cliente HTTP por nombre de clase:
  exige `UncaughtExceptionInterceptor`, `UserAgentInterceptor` y `CloudflareInterceptor`, y prohíbe `BrotliInterceptor`.
  También pisa `HttpSource.headers$delegate` por reflexión, así que `headers` tiene que ser `by lazy`.
- Injekt: se registran `Application`, `NetworkHelper` y `Json { ignoreUnknownKeys; explicitNulls = false }`.

## Prueba "Grand Blue" (2026-10-30)
| Fuente | Resultado |
|---|---|
| MangaDex | encontrado (sin capítulos en español) |
| MangaTV | 282 capítulos, 52 páginas en el cap. 1, imagen OK |
| LectorTMO (unoriginal) | 126 capítulos, 58 páginas, imagen webp OK |
| ManhwaWeb | encontrado |
| Toon-es | bloqueado por Cloudflare |
| Olympus, InManga | sin resultados |

## Búsqueda global "grand blue" con las 156 extensiones en español (165 fuentes)
- Carga de las 165 fuentes en ~0,6 s; búsqueda completa en ~25 s (lo limita el timeout de las fuentes lentas).
- "Grand Blue" aparece en ~20 fuentes (MangaTV, LectorTMO, ManhwaWeb, Kumanga, Zonatmo, TmoManhwa, LeerCapitulo, HeavenManga…).
- 31 fallos. Pendientes del host:
  - 12 × Cloudflare (sin WebView).
  - `android.webkit.CookieManager` (5), `android.util.LruCache`, `android.graphics.ColorMatrix`.
  - `Dispatchers.Main` no disponible (3): falta un dispatcher Main en la JVM.
  - `android.net.Uri.buildUpon()` / `Uri.Builder` (2) y un campo de `android.os.Build`.

## Salud de las fuentes (`reader/src/health.rs`, `~/.hantho/source_health.json`)
- Fallos seguidos por separado al buscar y al abrir: una fuente puede buscar bien y no dejar leer nada.
- Se omite en la búsqueda (salvo `hantho search --all`) tras 3 fallos seguidos, con espera creciente:
  red/timeout 30 min–6 h, Cloudflare/dominio muerto 6 h–3 días, HTTP/otros 1 h–1 día. Si falta una API de
  Android, tras 2 fallos y hasta 30 días o una versión nueva de Han Thỏ. Un 429, al primero, de 1 h a 1 día.
  Instalar la extensión borra sus fallos.
- Tandas: con más de 20 fuentes, cada búsqueda consulta 20 (favoritos, luego sin problemas, luego por latencia);
  `m` en la TUI pide las 20 siguientes, `all_sources` en MCP y `--all` en la CLI consultan todas.
- Cada búsqueda: máximo 8 peticiones a la vez y 1 por dominio (`search.rs`). Al vencer los 25 s se libera el hueco,
  pero la petición sigue en el host hasta su timeout (no hay operación de cancelar).
- No cuentan: los errores de red si ninguna fuente ha respondido en esa búsqueda (sin conexión), los del propio
  ext-host, ni un 404/410 al abrir (suele ser de ese manga, no de la fuente).
- Timeouts al abrir: 20 s (90 s con `r`). Los detalles siguen en segundo plano y se cachean si acaban llegando.

## Capítulos nuevos (`reader/src/news.rs`, `~/.hantho/new_chapters.json`)
- Solo se comprueba a petición (`u` en la biblioteca, `hantho updates`) y solo lo que está en Leyendo o Releyendo, de dos en dos.
- Se guardan los capítulos pendientes, no un contador: al leer, el número baja sin volver a consultar la fuente.

## Render
- ratatui-image escala con `FilterType::Nearest` y codifica Sixel con Floyd-Steinberg al 0,875, sin opción
  de cambiarlo: en manga produce moiré en las tramas y ruido. Por eso `reader/src/render.rs` hace su propio Sixel:
  Lanczos3 al tamaño exacto en píxeles + 64 grises sin dithering (páginas B/N) o 256 colores con dithering 0,3.
  Se escribe a stdout después de cada frame, en una zona donde ratatui no dibuja. Codificar una página tarda ~25 ms.
- ratatui-image se sigue usando para detectar el protocolo y el tamaño de celda, y como fallback (kitty/iterm2/halfblocks).
- Windows Terminal 1.24 soporta Sixel.
- Página de 1063x1506 escalada a 700px de ancho: ~1.1 MB de Sixel, ~0.5 s con el prototipo en Python (`tools/sixel_test.py`).

## Fuentes para Grand Blue (es)
- MangaDex: 0 capítulos en español (licenciado). Hay que buscar en sitios scan (Madara y similares).
