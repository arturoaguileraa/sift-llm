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
 [3] Detector                regex+validadores (MVP) → NER vía ort (fase 2)
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
| SSE (streaming del proveedor) | parseo manual o `eventsource-stream` |
| Vault cifrado en memoria | `aes-gcm` (RustCrypto) + `zeroize` |
| Config de políticas | `serde_yaml` |
| Mapa de sesión concurrente | `dashmap` |
| NER en proceso (fase 2) | `ort` (ONNX Runtime) con un modelo PII exportado |

## 6. El vault (bóveda de sesión)

Mapa bidireccional cifrado, por sesión, con TTL:

```rust
struct Vault {
    forward: HashMap<String, String>, // "juan@empresa.com" -> "[EMAIL_1]"
    reverse: HashMap<String, String>, // "[EMAIL_1]" -> "juan@empresa.com"
    counters: HashMap<String, u32>,   // por categoría, para numerar tokens
}
```

Reglas:

- **Coherencia.** Antes de crear un token se consulta `forward`; si el valor ya
  existe, se reutiliza. Así `[EMAIL_1]` es estable en toda la sesión y no se rompe
  el contexto multi-turno.
- **Formato de token.** `[TIPO_N]` con corchetes, deliberado para que el modelo lo
  trate como marcador y no lo manipule.
- **Ciclo de vida.** Nace con la sesión, muere con ella. En memoria, cifrado,
  borrado con `zeroize`. Nunca a disco sin cifrar. Nunca en logs.
- **Clave de sesión.** Derivada de algo estable de la conversación (id de sesión de
  opencode o hash de los primeros mensajes).

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

## 9. Streaming (la parte fina)

El rehidratador usa un buffer con ventana deslizante: acumula chunks, sustituye
tokens completos, y **retiene la cola que podría ser el inicio de un token a
medias** (`[EMA`) hasta que llegue el siguiente chunk. Los argumentos de tool_calls
llegan como deltas que forman un JSON: se bufferizan hasta estar completos antes de
rehidratar (no se rehidrata JSON parcial).

## 10. Problemas conocidos (documentados desde el diseño)

1. **Tensión proteger vs inutilizar.** En un agente de código el dato a veces ES la
   tarea. Política conservadora, `pass` generoso.
2. **El modelo transforma el token** (lo parte, lo pasa a mayúsculas) y la
   rehidratación por match exacto falla. Mitigación: formato de token que el modelo
   tiende a copiar tal cual.
3. **Tokenizar puede romper la sintaxis del código.**
4. **Falsos positivos/negativos.** Umbral de confianza + modo shadow.
5. **El vault es la joya.** Cifrado en memoria, TTL, zeroize, cero logs del body.
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
- **Fase 3 — Vault reversible.** Pseudonimización coherente (tokens `[TIPO_N]`
  estables por sesión) + **rehidratación de la respuesta** sobre streaming (aquí sí
  entra el buffer de ventana deslizante). Es el salto de "redacción" a
  "anonimización reversible".
- **Fase 4 — Tools.** Rehidratar los argumentos de los tool_calls + escanear los
  tool_results como fuente principal de PII.
- **Fase 5 — NER (`ort`).** Detección semántica de nombres/direcciones más allá de
  los patrones regex.
- **Fase 6 — Multiproveedor.** Adaptador canónico para Anthropic además de OpenAI.
- **Futuro opcional.** Multimedia (OCR + block), modo `ask` interactivo.

## 12. Estructura del repo

```
sift-llm/
├── Cargo.toml
├── src/
│   ├── main.rs            # arranque, axum, rutas
│   ├── proxy.rs           # recepción + reenvío con streaming
│   ├── schema/            # adaptadores (openai.rs, anthropic.rs, canonical.rs)
│   ├── detect/            # regex.rs, entropy.rs, ner.rs (fase 2)
│   ├── policy.rs          # carga de config + motor de decisiones
│   ├── vault.rs           # mapa bidireccional cifrado por sesión
│   ├── rehydrate.rs       # buffer de ventana deslizante
│   └── audit.rs           # registro
├── policies.yaml
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

# registrar proveedores: picker interactivo (populares + URL personalizada)
sift provider add                       # menú interactivo
sift provider add --url https://api.groq.com/openai/v1   # endpoint OpenAI-compatible

# ver los modelos expuestos al agente, cada uno etiquetado "(Sift secured)"
sift models
```

### Superficie de CLI

| Comando | Qué hace |
|---|---|
| `sift serve --config policies.yaml` | Arranca el gateway (el proxy). Daemon persistente en `localhost:8787`. **Es el producto.** |
| `sift provider add` | Registra un proveedor upstream. Picker interactivo (populares + URL personalizada), o con flags `--url` / `--key-env` / `--api-key`. Descubre modelos y re-sincroniza opencode. |
| `sift provider list` | Lista los proveedores registrados. |
| `sift provider remove <name>` | Quita un proveedor. Re-sincroniza opencode. |
| `sift sync-opencode` | Escribe los modelos del registro en la config de opencode (provider `sift-llm`). Se ejecuta solo en add/remove; `--path` cambia la ruta. |
| `sift models` | Lista los modelos expuestos al agente, cada uno con `(Sift secured)`. |
| `sift status` | Indica si el gateway está corriendo (y su PID). |
| `sift scan <file>` | Diagnóstico puntual: muestra qué se detectaría/redactaría. No es el proxy. |

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
2. El NER en fase 2 se hace en proceso con `ort` (ONNX Runtime), mejor soportado en
   Rust que en Go.
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
