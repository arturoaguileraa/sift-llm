# Sift (sift-llm) — Proxy reverso de anonimización reversible para harnesses de IA

> Nombre de trabajo: Sift (sift-llm).

Middleware local que se sitúa entre un harness open source (opencode como primer
objetivo) y la API de cualquier modelo LLM. Se hace pasar por el endpoint del
proveedor, intercepta las peticiones, **pseudonimiza datos sensibles de forma
reversible** antes de que salgan de la máquina, y **rehidrata** la respuesta para
que siga siendo útil.

Lenguaje elegido: **Rust**.

---

## 1. Qué es

Un proxy reverso local, agnóstico de harness (se engancha cambiando un `baseURL`)
y agnóstico de modelo (traduce cada proveedor a un esquema interno canónico). El
harness no sabe que hay algo en medio.

## 2. Qué NO es (límites conscientes)

- **No inspecciona multimedia.** En esta fase las imágenes pasan sin tocar.
  Fase futura opcional: detectar y bloquear, nunca tokenizar píxeles.
- **No controla la ejecución local de tools.** Si una tool hace su propia
  petición de red (`curl`, un servidor MCP saliente), eso ocurre dentro del
  harness y queda fuera de alcance. Eso es control de egress de red, otro proyecto.
- **No es un firewall ni un antivirus.** Solo ve tráfico LLM.

## 3. Principio de funcionamiento

```
opencode ──baseURL=localhost──► [ SIFT ] ──clave real──► API del modelo
   ▲                                │
   └──── respuesta rehidratada ◄────┘
```

opencode cree que habla con la API real. Sift guarda la clave verdadera, aplica el
pipeline y reenvía. La conversación fluye sin que opencode note nada.

## 4. Arquitectura (pipeline)

```
PETICIÓN:
 [1] Adaptador de esquema    OpenAI/Anthropic → forma canónica interna
 [2] Extractor de contenido  system, messages, tool defs, tool_results, imágenes
 [3] Detector                regex (estructurado) + NER GLiNER vía ort (semántico)
 [4] Motor de políticas       pass / pseudonymize / block / ask, por categoría
 [5] Pseudonimizador          consulta/escribe el VAULT, sustituye en el body
 [6] Reserializador           vuelve al esquema del proveedor y reenvía

RESPUESTA (streaming):
 [7] Rehidratador             buffer de ventana deslizante; sustituye tokens→valores
                              en TEXTO y en argumentos de tool_calls
 [8] Auditoría                registro de qué se detectó y qué política se aplicó
```

## 5. Stack técnico (Rust)

| Función | Crate |
|---|---|
| Runtime async | `tokio` |
| Servidor HTTP | `axum` (sobre `hyper`/`tower`) |
| Cliente HTTP (reenvío con streaming) | `reqwest` (feature `stream`) |
| Parseo/serialización JSON | `serde` + `serde_json` |
| Detección regex | `regex` |
| Detección semántica (NER) | `gline-rs` (GLiNER) sobre `ort` (ONNX Runtime), modelo `gliner_small-v2.1` int8; ONNX Runtime **linkado estático** para un binario único autocontenido |
| SSE (streaming del proveedor) | parseo manual de eventos `data:` |
| Vault en memoria | `zeroize` (borra la PII al soltar el vault por petición) |
| Config de políticas | `serde_yaml` |

## 6. El vault

Mapa bidireccional entre valor real y token:

```rust
struct Vault {
    forward: HashMap<String, String>, // "juan@empresa.com" -> "[EMAIL_1]"
    reverse: HashMap<String, String>, // "[EMAIL_1]" -> "juan@empresa.com"
    counters: HashMap<String, u32>,   // por categoría, para numerar tokens
}
```

Reglas:

- **Coherencia.** Antes de crear un token se consulta `forward`; si el valor ya
  existe, se reutiliza. Así `[EMAIL_1]` es estable dentro de la petición.
- **Formato de token.** `[TIPO_N]` con corchetes, deliberado para que el modelo lo
  trate como marcador y no lo manipule.

### Decisión de alcance: vault por petición, no por sesión (implementado)

El vault es **efímero, por petición**: se llena al tokenizar la petición saliente y se
vacía al rehidratar la respuesta, todo dentro del mismo handler HTTP. **No hace falta un
store de sesión persistente**, y esta es la parte contraintuitiva del diseño:

- **Correctitud.** Como rehidratamos la respuesta, el harness (opencode) solo ve
  valores reales y los reenvía en cada turno. Cada petición re-tokeniza desde cero; el
  token solo necesita vivir durante ese viaje ida/vuelta.
- **Prompt caching.** La tokenización es determinista **por orden de primera
  aparición**. En un turno multi-mensaje, el prefijo (turnos anteriores, sin cambios) se
  recorre en el mismo orden y produce los mismos tokens, así que el prefijo cacheable del
  proveedor se mantiene estable byte a byte **sin** vault persistente.

Un vault **por sesión** (cifrado, con TTL, `zeroize`, clave derivada del id de sesión o
del hash de los primeros mensajes) solo sería necesario para un harness que **no**
reenvíe el historial completo, sino solo el mensaje nuevo (APIs con estado en servidor,
tipo *threads*). No es el caso de opencode. Además, un vault persistente vive más y es un
objetivo más goloso, así que persistir sería un *trade-off* de seguridad, no una mejora
gratis. Por eso queda **fuera de la ruta principal** (ver Fase 3b en el roadmap).

## 7. Motor de políticas

Configurable por el usuario, por categoría, con acción:

```yaml
policies:
  api_key:     block         # secretos: irreversible, más seguro
  password:    block
  credit_card: block
  email:       pseudonymize  # reversible, se rehidrata
  person_name: pseudonymize
  ip_address:  pass          # funcional en coding, se deja pasar
allowlist:
  - "example.com"
  - "127.0.0.1"
thresholds:
  ner_confidence: 0.85
mode: shadow                 # shadow (solo audita) | enforce (actúa)
```

Regla de oro: **secreto = block, PII que necesitas de vuelta = pseudonymize, dato
funcional = pass.** El `mode: shadow` es obligatorio al principio para calibrar sin
romper nada.

## 8. Flujo con tools

El uso de tools es un protocolo compartido entre modelo y harness. El pipeline lo
trata en tres puntos:

- **tool_call args (respuesta del modelo).** El rehidratador [7] entra también en
  los `arguments` JSON, no solo en el texto. Si no, opencode ejecutaría la tool con
  un token literal (`send_email(to="[EMAIL_1]")`).
- **tool_result (petición siguiente).** El detector [3] los trata como fuente
  **principal** de PII, porque en coding los secretos entran cuando el agente lee
  ficheros.
- **ejecución de la tool.** Punto ciego, fuera de alcance.

**Importante: NO se redactan las DEFINICIONES de tools.** El detector recorre el
contenido de la conversación (texto de mensajes, `arguments` de tool_calls), pero
**salta** las claves estructurales (`tools`, `functions`, `tool_choice`,
`response_format`). Tokenizar una palabra dentro del esquema JSON de una función
(p. ej. un nombre de propiedad en `required`) corrompe la petición y el proveedor la
rechaza (`schema ... requires unspecified property '[PERSON_NAME_1]'`). Esto lo destapó
el NER, que matchea palabras comunes; el regex casi nunca lo hacía.

### Passthrough de estado opaco del proveedor (thought_signature)

Algunos proveedores adjuntan a los tool_calls datos opacos que deben devolverse tal
cual en el turno siguiente. El caso guía: los modelos **Gemini 3 "thinking"** ponen un
`extra_content.google.thought_signature` en cada function call y rechazan la
continuación (`400 "missing thought_signature"`) si no vuelve. Los clientes
OpenAI-compat genéricos (opencode) tiran ese campo. Como Sift ve la respuesta,
**captura** ese `extra_content` (indexado por `id` del tool_call) y lo **re-inyecta** en
el tool_call correspondiente de la siguiente petición. Así funcionan esos modelos a
través de la superficie OpenAI-compat de Sift **sin** reimplementar el protocolo nativo
de cada proveedor (módulo `signature.rs`).

## 9. Streaming (la parte fina)

El `SseRehydrator` trabaja a dos niveles porque un token se puede partir de dos formas:

1. **Framing de transporte.** Un chunk de red puede cortar un evento SSE (o un carácter
   UTF-8 multibyte) por la mitad. Se bufferizan bytes crudos y solo se procesan eventos
   completos, delimitados por la línea en blanco `\n\n`.
2. **Framing de delta.** El modelo emite un token `[EMAIL_1]` en varios trozos de
   `delta.content`, cada uno en su propio evento, así que **el token nunca está contiguo
   en los bytes**. Se reensambla a nivel de texto: una ventana deslizante acumula el
   fragmento a medias en `pending` y lo libera (rehidratado) al completarse el token,
   reteniendo la cola que podría ser el inicio de un token (`[EMA`).

**Tool_calls sobre streaming.** Los `arguments` llegan como fragmentos de un string JSON.
En vez de bufferizar el JSON completo (que retrasaría la emisión), se aplica la **misma**
ventana deslizante pero **por cada tool_call** (buffer `pending_args` indexado por el
`index` del call). Un `[` de un array JSON legítimo nunca se confunde con un token porque
el patrón de token exige mayúsculas + `_dígitos]` (`[TIPO_N]`); a lo sumo se retiene un
instante hasta el siguiente fragmento. Caveat: como la sustitución es un splice de texto,
un valor con un metacarácter JSON (una comilla dentro de una password) podría romper un
frame; emails/nombres/IPs no se ven afectados.

## 10. Problemas conocidos (documentados desde el diseño)

1. **Tensión proteger vs inutilizar.** En un agente de código el dato a veces ES la
   tarea. Política conservadora, `pass` generoso.
2. **El modelo transforma el token** (lo parte, lo pasa a mayúsculas) y la
   rehidratación por match exacto falla. Mitigación: formato de token que el modelo
   tiende a copiar tal cual.
3. **Tokenizar puede romper la sintaxis del código.**
4. **Falsos positivos/negativos.** Umbral de confianza + modo shadow.
5. **El vault es la joya.** Hoy: efímero por petición, borrado con `zeroize` al soltarlo,
   cero logs del body. Cifrado en memoria + TTL solo aplicarían al vault por sesión (3b).
6. **Rompe el prompt caching del proveedor** si el contenido muta; la coherencia
   del vault ayuda pero hay que vigilar coste.
7. **Punto ciego de egress** en la ejecución de tools.
8. **Mantener el adaptador canónico** al día con cada versión de las APIs.

## 11. Roadmap por fases

El punto de partida es **replicar el Local Privacy Proxy de William Ogou** (proxy
local con redacción regex irreversible, una sola dirección) y a partir de ahí ir
añadiendo features hasta la anonimización reversible completa.

- **Fase 1 — Réplica de Local Privacy Proxy (Ogou).** Proxy que expone
  `/v1/chat/completions`, aplica **detección regex** (30+ patrones: claves API,
  emails, cadenas de conexión, IPs, rutas) sobre la petición y **redacta de forma
  irreversible** con etiquetas fijas (`[EMAIL_REDACTED]`, `[API_KEY_REDACTED]`).
  La respuesta se reenvía **sin tocar** (una sola dirección, como el original), así
  que el streaming es passthrough puro y no hace falta buffer todavía. opencode
  enganchado y funcionando. Esto es ya un producto útil por sí solo.
- **Fase 2 — Motor de políticas.** Elevar la redacción fija a reglas por categoría
  (`pass` / `redact` / `block`), allowlist/denylist, umbral de confianza y modo
  `shadow` (solo audita) vs `enforce`.
- **Fase 3a — Vault reversible (IMPLEMENTADA).** Pseudonimización coherente (tokens
  `[TIPO_N]` estables dentro de la petición) + **rehidratación de la respuesta**, tanto
  buffered como sobre streaming. El rehidratador SSE reensambla tokens partidos entre
  eventos `delta` y entre chunks de transporte (mantiene el fragmento a medias hasta que
  el token se completa; flush del pendiente en `[DONE]`). Vault **efímero por petición**
  (ver §6). Es el salto de "redacción" a "anonimización reversible".
- **Fase 3b — Vault por sesión (fuera de la ruta principal).** Solo necesario para
  harnesses que no reenvían el historial completo (APIs con estado). Vault cifrado, con
  TTL y `zeroize`. Ver la discusión en §6.
- **Fase 4 — Tools (IMPLEMENTADA).** Rehidratación de los `arguments` de los tool_calls
  en buffered **y** en streaming: en SSE cada tool_call se reensambla en su propio buffer
  por `index`, deslizando los fragmentos de `arguments` igual que el `content` (un `[` de
  array JSON nunca se confunde con un token `[TIPO_N]`). Los tool_results de la petición
  ya se escanean como cualquier otro campo por la redacción recursiva. Punto ciego que
  queda: la ejecución de la tool (egress fuera del modelo), fuera de alcance.
- **Fase 5 — NER (IMPLEMENTADA).** Detección semántica de nombres/organizaciones/
  ubicaciones más allá del regex, con un modelo **GLiNER** (`gliner_small-v2.1` int8)
  ejecutado en local vía `gline-rs`/`ort`. Compuesta con el regex (regex = secretos
  estructurados; NER = PII contextual). ONNX Runtime va **linkado estático** → `sift`
  sigue siendo un binario único autocontenido (~29 MB); el modelo (~183 MB) se descarga
  bajo demanda (`sift model pull`, o automático en install/primer arranque) a
  `~/.config/sift/models/gliner`; si falta, degrada a solo-regex. El `person_name` de
  `policies.yaml` ya funciona. Pendiente: umbral configurable (hoy 0.5), y medir/optimizar
  la latencia sobre system prompts grandes.
- **Fase 6 — Multiproveedor / protocolos nativos.** Hoy Sift habla **solo
  OpenAI-compat** (`/v1/chat/completions`), y por eso Gemini pasa por su endpoint compat
  (que exige el `thought_signature` que ya reinyectamos, ver §8). Pendiente: adaptadores
  de protocolo **nativo** (Gemini `generateContent`, Anthropic `/v1/messages`).
- **Futuro opcional.** Multimedia (OCR + block), modo `ask` interactivo.

## 12. Estructura del repo

Estructura real actual (no la aspiracional; aún no hay `schema/` canónico porque solo
se habla OpenAI-compat):

```
sift-llm/
├── Cargo.toml
├── .cargo/config.toml     # fuerza ONNX Runtime estático (evita el del sistema)
├── src/
│   ├── main.rs            # CLI (clap), arranque, comandos, model pull, auto-download
│   ├── proxy.rs           # axum, /v1/chat/completions, redacción + rehidratación + reenvío
│   ├── detect/
│   │   ├── mod.rs         # Detector compuesto (regex + NER opcional)
│   │   ├── regex.rs       # patrones estructurados (claves, email, IBAN, connection string…)
│   │   └── ner.rs         # NerDetector (GLiNER vía gline-rs/ort)
│   ├── policy.rs          # config YAML + motor pass/redact/pseudonymize/block, shadow/enforce
│   ├── vault.rs           # mapa bidireccional por petición (zeroize al soltar)
│   ├── rehydrate.rs       # rehidratación buffered + SseRehydrator (ventana deslizante)
│   ├── signature.rs       # passthrough de thought_signature (§8)
│   ├── provider.rs        # registro multiproveedor + descubrimiento de modelos
│   ├── opencode.rs        # sync del provider en la config de opencode
│   └── audit.rs           # logs ↑ pseudonimizar / ↓ rehidratar
├── policies.yaml
├── install.sh             # descarga binario + modelo NER
└── README.md
```

## 13. Instalación y uso

```bash
# Instalación binaria directa (macOS / Linux):
curl -fsSL https://raw.githubusercontent.com/arturoaguileraa/sift-llm/main/install.sh | bash

# O desde código fuente:
git clone https://github.com/arturoaguileraa/sift-llm.git && cd sift-llm
cargo build --release

# arrancar el gateway (daemon persistente en :8787)
export ANTHROPIC_API_KEY=sk-ant-...
sift serve --config policies.yaml

# registrar proveedores: picker con flechas (populares + URL personalizada)
sift provider add                       # menú interactivo
sift provider add --url https://api.groq.com/openai/v1   # endpoint OpenAI-compatible

# ver los modelos expuestos al agente, cada uno etiquetado "(Sift secured)"
sift models
```

### Superficie de CLI

| Comando | Qué hace |
|---|---|
| `sift serve --config policies.yaml` | Arranca el gateway (el proxy) en `localhost:8787`. **Es el producto.** `-d`/`--daemon` lo lanza en segundo plano; `--port` cambia el puerto. |
| `sift stop` | Para un gateway en segundo plano lanzado con `--daemon`. |
| `sift provider add` | Registra un proveedor upstream. Picker con flechas (populares + URL personalizada), o con flags `--url` / `--key-env` / `--api-key`. Descubre modelos y re-sincroniza opencode. |
| `sift provider list` | Lista los proveedores registrados. |
| `sift provider remove <name>` | Quita un proveedor. Re-sincroniza opencode. |
| `sift sync-opencode` | Escribe los modelos del registro en la config de opencode (provider `sift-llm`). Se ejecuta solo en add/remove; `--path` cambia la ruta. |
| `sift models` | Lista los modelos expuestos al agente, cada uno con `(Sift secured)`. |
| `sift status` | Indica si el gateway está corriendo (y su PID). |
| `sift scan <file>` | Diagnóstico puntual: muestra qué se detectaría/redactaría. No es el proxy. |
| `sift uninstall` | Elimina config, el provider `sift-llm` de opencode, el bloque de PATH y el binario. `--yes` sin prompt; `--keep-binary` conserva el binario. |

El registro **arranca vacío**: solo los proveedores que añades se exponen, nada se
siembra por defecto.

Distinción clave: **`serve` es el proxy** (invisible, corre en segundo plano e
intercepta todo el tráfico); **`scan` es una utilidad de diagnóstico** de un disparo
para calibrar políticas. No se usa `scan` por cada prompt.

**Integración con opencode:** opencode NO auto-descubre los modelos de un provider
custom OpenAI-compatible; hay que listarlos en `opencode.jsonc`. Sift lo hace por ti
con `sift sync-opencode` (y automáticamente al añadir/quitar providers), escribiendo
solo el bloque `models` del provider `sift-llm` y preservando el resto de tu config.
Tras un sync hay que reiniciar opencode para que relea.

```json
// opencode.json — enganche, sin tocar opencode
{
  "$schema": "https://opencode.ai/config.json",
  "provider": {
    "safe": {
      "npm": "@ai-sdk/openai-compatible",
      "name": "Modelo protegido (Sift)",
      "options": { "baseURL": "http://localhost:8787/v1" },
      "options.apiKey": "{env:REAL_API_KEY}",
      "models": { "claude-sonnet-4-6": { "name": "Claude (Sift)" } }
    }
  }
}
```

---

## Decisión de lenguaje (registro)

Se evaluó Go vs Rust. Se eligió **Rust** por tres razones para este proyecto:

1. El rehidratador sobre streaming es una máquina de estados con tokens partidos
   entre chunks; los enums y el pattern matching de Rust lo modelan sin errores.
2. El NER se ejecuta en proceso con `ort` (ONNX Runtime), mejor soportado en Rust que
   en Go, y linkable estático en un binario único (se confirmó en la práctica).
3. Se maneja un vault con PII en memoria; Rust permite borrarlo de forma
   determinista (`zeroize`) sin recolector de basura.

Coste: iteración más lenta al principio. Si la prioridad fuera velocidad de
entrega, Go sería la elección.

## Decisión de patrón de despliegue (registro)

Existen dos patrones en el mercado:

- **Patrón A — Wrapper/launcher** (ej. `og-local`): se invoca como `ogl claude "..."`,
  levanta un proxy en un puerto de loopback **aleatorio y efímero** por ejecución,
  envuelve al agente como proceso hijo y hace passthrough transparente de
  credenciales. Cero config, invisible, sin registro de modelos.
- **Patrón B — Gateway persistente** (ej. LiteLLM, Kong, Portkey): servidor
  persistente en una **IP/puerto fijo**, con **registro de modelos** (enrutado por el
  campo `model`), **claves centralizadas** en el proxy y política aplicada a
  cualquier cliente que apunte al endpoint.

**Decisión: Patrón B puro para el MVP.** Razones: es la forma que valida la industria
para control centralizado + políticas + auditoría, encaja de forma natural con el
motor de políticas y el audit trail, y es el diferenciador frente a og-local (que es
patrón A). Es además la elección más reconocible para portfolio.

**Requisito de diseño derivado:** el **core** (pipeline de detección, políticas,
vault, rehidratación) debe quedar **desacoplado del modo de arranque**, de forma que
en el futuro se pueda añadir un **modo wrapper (patrón A)** opcional encima sin
reescribir el core. No entra en el MVP.

Riesgos asumidos (patrón B): mercado disputado (los incumbentes ya hacen gateway +
PII), el proxy concentra claves reales + vault de PII (objetivo goloso, punto único
de fallo, listón de seguridad alto), y carga operativa de un servicio persistente.
Por eso "gateway + PII" no basta como diferencial: hace falta un **wedge** más
afilado (pendiente de fijar). Candidatos: reversibilidad de alta calidad específica
para agentes de código; políticas granulares como producto (shadow, allowlist,
umbral, versionadas); self-hosted/open source de verdad; o foco vertical en datos
regulados.
