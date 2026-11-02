# cgpt-gateway — план реализации

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Локальный OpenAI-совместимый шлюз, через который OpenCode работает с веб-ChatGPT во вкладке Chrome через мост cgpt-bridge — на Mac и на Windows (обе платформы целевые, решение пользователя 26.10.2026).

**Architecture:** Новый крейт `gateway/` (Rust, `tiny_http`) принимает `/v1/chat/completions` от OpenCode. Он переводит историю и инструменты в сообщения ChatGPT, держит сессию в своём чате и разбирает ответ в `tool_calls`. Мост дорабатывается: адресация чата, своя вкладка, `stop`/`status`/`rate_limited`, named pipe на Windows. Из ветки `feat/cgpt-desk-core` остаются только вычистка секретов и denylist в крейте `core/`.

**Tech Stack:** Rust (workspace), serde/serde_json, regex, `tiny_http`, `interprocess` (локальный сокет: Unix-сокет / named pipe), `getrandom` (токен), TypeScript-расширение Chrome (MV3), PowerShell для установки, GitHub Actions `windows-latest`.

**Spec:** `docs/superpowers/specs/2026-10-26-opencode-gateway-design.md`. Заменяет план `2026-10-23-cgpt-desk-01-core.md`: тот план остановлен на задаче 6.

## Global Constraints

- Шлюз слушает только `127.0.0.1`. Каждый запрос — `Authorization: Bearer <токен>`, иначе 401. Порт по умолчанию — `8765`. Конфиг — `<data_dir>/cgpt-gateway/config.json`: `{ port, token, turn_timeout_ms: 900000, allow_tools: [] }`. Токен создаётся при первом запуске: 32 случайных байта в hex.
- Модели в `/v1/models`: `chatgpt-web` (через ChatGPT) и `local-title` (локально, без ChatGPT).
- Запросы к ChatGPT — строго по одному. Остальные ждут в очереди; ожидание входит в таймаут хода (по умолчанию 900 000 мс).
- SSE: сразу кусок `role: "assistant"`, keepalive-комментарий `: keepalive` каждые 10 с, потом кусок с `content` или `tool_calls`, кусок с `finish_reason` + `usage`, затем `data: [DONE]`.
- Ответ ChatGPT с вызовами — ровно один fenced-блок `tool_calls` с JSON-массивом `[{"name": "...", "arguments": {...}}]`, метка без учёта регистра. Без блока — текст. Одна автоматическая просьба исправить за ход.
- `tool_call.id` — `call_<8 hex сессии>_<n>`. `usage` — символы / 4.
- Одно сообщение в ChatGPT — до `MAX_MESSAGE_CHARS = 60_000` символов. Длиннее — части «часть N из M, ответь словом ok».
- Пакет ввода в курс дела — не больше 2 частей (120 000 символов):
  - все сообщения пользователя и текст ассистента;
  - последние 3 результата инструментов целиком;
  - более старые результаты обрезаются до 2 000 символов;
  - если всё ещё не помещается — отбрасываются самые старые ходы с пометкой «N ранних ходов опущено».
- Ключ сессии — заголовок `x-opencode-session-id`, без него — хэш первого сообщения пользователя. Сессии хранятся в `<data_dir>/cgpt-gateway/sessions/<key>.json`, без обращений больше 7 дней удаляются.
- Сначала `core::redact`, потом деление на части. Действует для всего, что уходит в ChatGPT.
- Denylist применяется к аргументу `command` любого инструмента со строковым параметром `command`. На `Blocked` вызов не отдаётся OpenCode, а ChatGPT получает отказ с именем правила (один раз за ход). Второй запрещённый вызов в том же ходе превращается в текст для пользователя.
- Инструмент `execute` убирается из списка для ChatGPT, если его нет в `allow_tools`.
- Аргументы вызова с маркером вычищенного секрета не отдаются OpenCode, ChatGPT получает просьбу переписать без маркера.
- Паузы после `rate_limited`: 5 мин, повтор за час — 15, затем 30. Во время паузы — сразу 429 с текстом «ChatGPT ограничил частоту запросов, повторите после ЧЧ:ММ».
- Отображение ошибок моста: нет расширения или вкладки → 503, `rate_limited` → 429, таймаут → 504, `dom_failure` → 502.
- Запросы моста без цели (CLI `cgpt ask`/`agent`) ведут себя как раньше: активная вкладка пользователя.
- Новые зависимости — только из Tech Stack.

## Review Focus

1. **OpenCode повторяет запрос после «Стоп» или таймаута.** В чат не уходит дубль, в начале доставки — пометка «предыдущее сообщение прервано», пакет ввода в курс дела заново не шлётся. Тест в задаче 8.
2. **Две сессии одновременно** — основная и подагент, или две вкладки OpenCode. Они не смешиваются: каждая в своём чате, запросы по очереди. Тест в задаче 8.
3. **В ответе ChatGPT несколько блоков кода** (` ```json ` с примером и ` ```tool_calls `). Учитывается только блок `tool_calls`; два блока `tool_calls` — просьба исправить. Тест в задаче 7.
4. **Огромный результат инструмента** (OpenCode прочитал 200 КБ). Вычищается, затем делится; каждая часть не больше предела. Тест в задаче 7.
5. **Кириллица и эмодзи** в тексте и в аргументах вызовов. Корректный UTF-8 в SSE и JSON, без разрыва символов. Тест в задаче 6.

---

## Файлы

```
core/                          крейт cgpt-bridge-core: redact, denylist (остаётся), limits (MAX_MESSAGE_CHARS)
protocol/src/message.rs        BridgeRequest/Response: Ask с target, Stop, Status; ErrorCode::RateLimited (задача 3)
protocol/src/local_socket.rs   имя/путь локального сокета и подключение (задача 5; заменяет socket_path.rs)
host/src/main.rs               прозрачная пересылка всех типов, слушатель через interprocess (задачи 3, 5)
cli/src/transport.rs           клиент через interprocess (задача 5)
extension/src/bridge.ts        своя вкладка, адресация, stop/status (задача 4)
extension/src/chatgptAdapter.ts новый чат, переход в чат, кнопка «Стоп», признак rate limit (задача 4)
extension/src/pageShim.ts      сообщение о HTTP 429 потока (задача 4)
gateway/                       новый крейт cgpt-gateway (задачи 6–9)
  src/main.rs                  запуск, конфиг, сервер
  src/config.rs                загрузка и создание конфига, токен
  src/http.rs                  маршруты, авторизация, SSE и JSON-ответы, keepalive
  src/openai.rs                типы запроса и ответа OpenAI chat-completions
  src/backend.rs               трейт ChatBackend + BridgeBackend (через мост) + FakeBackend (тесты)
  src/prompt.rs                правила шлюза, компактные инструменты, форматирование истории и дельты, деление
  src/reply.rs                 разбор ответа ChatGPT: блок tool_calls, проверка по схемам, просьба исправить
  src/session.rs               ключ, отпечатки, состояния, хранение, истечение
  src/safety.rs                denylist для command, маркер, фильтр execute
  src/cooldown.rs              паузы после rate limit
  src/title.rs                 локальный заголовок
install/windows/install.ps1, uninstall.ps1, opencode-provider.json   (задача 11)
install/macos/install.sh, uninstall.sh, com.cgpt-bridge.gateway.plist  (задача 11)
.github/workflows/release.yml  (задача 11: windows-latest + macos-latest)
```

---

### Задача 1: расчистка ветки

**Files:** git-операции; `docs/superpowers/specs/2026-10-23-windows-agent-app-design.md` (пометка в начале).

- [ ] **Step 1:** В ветке `feat/cgpt-desk-core` закоммитить незаконченную правку `core/src/denylist/tests.rs` сообщением `wip(core): interrupted fix round 2 tests (superseded by cgpt-gateway plan)`.
- [ ] **Step 2:** Создать ветку `feat/cgpt-gateway` от `main` и перенести `git cherry-pick` три коммита:
  - `be99c11` — крейт core;
  - `69c578c` и `e372bb3` — denylist PowerShell и cmd.

  Конфликты в `core/src/lib.rs` разрешить так, чтобы остались только `denylist` и `redact`.
- [ ] **Step 3:** Добавить `core/src/limits.rs` с единственной константой `pub const MAX_MESSAGE_CHARS: usize = 60_000;` и `pub mod limits;`.
- [ ] **Step 4:** Перенести из `feat/cgpt-desk-core` коммит `552673f` (игнор `.superpowers/`), если его нет. В начало старой спецификации добавить строку «Заменён: `2026-10-26-opencode-gateway-design.md`».
- [ ] **Step 5:** `cargo test --workspace`. Ожидается: PASS, нет `agent_v2`, `workspace`, `tools`.
- [ ] **Step 6:** Commit `chore: start cgpt-gateway branch with core redact and denylist only`.

### Задача 2: denylist — надёжность для шлюза

**Files:** `core/src/denylist/*`

**Interfaces:** `classify(command) -> Classification` без изменения сигнатуры.

- [ ] **Step 1: Write the failing tests:**
  - `classify_never_panics_on_tricky_input` — `İ`, `ẞ`, комбинирующие знаки, пустая строка, одиночная кавычка или обратный апостроф, 10 КБ `(`, `"Get-Content -İ:жж"`;
  - `nested_wrappers_are_bounded` — `"powershell "*4000 + "ls"` возвращается быстрее 1 с и даёт `Unknown`;
  - `downloads_without_execution_not_blocked` — не `Blocked`:
    - `Invoke-WebRequest https://x/setup.exe -OutFile .\setup.exe`;
    - `curl -L https://x/tool.exe -o ./bin/tool.exe`;
    - `dotnet run & curl http://localhost:5000`;
    - `npm start & sleep 2; curl localhost:3000`;
    - `cd src & curl -O https://x/a.zip`;
    - `curl https://api.iex.cloud/v1/data`;
  - `download_then_execute_blocked` — `Blocked`:
    - `iwr x -OutFile a.ps1; .\a.ps1`;
    - `iwr x -OutFile a.ps1; powershell -File a.ps1`;
    - `$s = iwr x; iex $s`;
  - `quoted_and_called_command_names_blocked` — `"reg" delete HKLM\Software\X /f`, `& "reg.exe" delete …`, `"C:\Windows\System32\reg.exe" delete …`, `"vssadmin" delete shadows /all`, `"format" D:`, `& "format.com" D: /q`;
  - `dash_and_slash_spellings_blocked`:
    - `powershell /enc AAAA`, `pwsh -ep Bypass -enc AAAA`, `powershell –enc AAAA` (en dash);
    - `Remove-Item –Recurse –Force C:\`;
    - `cmd /c rd/s/q C:\`, `cmd /d/c rd /s /q C:\`;
    - `${env:SystemDrive}` как цель рекурсивного удаления;
  - `subexpression_payload_blocked` — `Write-Output "$(Remove-Item -Recurse -Force C:\)"`, `echo "$(rm -rf ~)"`;
  - `comma_array_paths_not_readonly` — `Get-Content x,/etc/passwd`, `Get-ChildItem x,~ -Recurse`, `Select-String -Path x,~\* -Pattern AKIA`;
  - `man_less_not_readonly` — `man -P "rm -rf x" ls`.
- [ ] **Step 2:** `cargo test -p cgpt-bridge-core denylist`. Ожидается: FAIL на новых тестах.
- [ ] **Step 3:** Исправить.
  - Убрать всё «индекс в lowercase → срез оригинала».
  - Предел глубины разворачивания обёрток — 8.
  - Исполнение в `download_and_execute` определять по токену команды, а не по подстроке. POSIX `&` — это фон, а не вызов.
  - Нормализовать `–`/`—`, `/`-префиксы параметров powershell, `-ep`, `-Args`.
  - Первый токен очищать от `&`, кавычек, каталога и `.exe`/`.com`.
  - Делить аргументы на элементы массива по `,` и проверять каждый.
  - Проверять блок-правилами содержимое `$(…)`.
  - Убрать `man`, `less` из «только чтения».
- [ ] **Step 4:** `cargo test --workspace` — PASS.
- [ ] **Step 5:** Commit `fix(core): denylist panics, bounded recursion, precise download-execute, more spellings`.

### Задача 3: протокол моста — адресация, stop, status

**Files:** `protocol/src/message.rs`, `host/src/main.rs`, `host/tests/router_e2e.rs`, `cli/src/transport.rs` (только новые поля `None`)

**Interfaces — Produces:**

```rust
pub enum ChatTarget { #[serde(rename = "conversation")] Conversation { url: String }, #[serde(rename = "new_chat")] NewChat }
// serde: { "kind": "conversation", "url": "..." } | { "kind": "new_chat" }
pub struct AskRequest { pub id: String, pub text: String, pub timeout_ms: u64, #[serde(default)] pub target: Option<ChatTarget> }
pub enum BridgeRequest { Ping(PingRequest), Ask(AskRequest), Stop(StopRequest /* {id} */), Status(StatusRequest /* {id} */) }
pub enum BridgeResponse { Pong{..}, AskResult { id, text, #[serde(default)] conversation_url: Option<String> },
                          Stopped { id, was_generating: bool }, StatusResult { id, extension: bool, tab_open: bool, logged_in: bool, conversation_url: Option<String> },
                          Error{..} }
pub enum ErrorCode { …, RateLimited /* новый */ }
```

- [ ] **Step 1: Write the failing tests:**
  - `ask_with_target_roundtrip` — сериализация в оба вида `target`;
  - `ask_without_target_is_backward_compatible` — старый JSON CLI разбирается с `target: None`;
  - `ask_result_conversation_url_optional`;
  - `stop_and_status_roundtrip`;
  - `rate_limited_code_snake_case`;
  - `host_forwards_stop_and_status` (router_e2e) — мок-расширение получает кадр `stop` с тем же id, ответ `stopped` возвращается клиенту.
- [ ] **Step 2:** `cargo test -p cgpt-bridge-protocol -p cgpt-bridge-host`. Ожидается: FAIL.
- [ ] **Step 3:** Реализовать.
  - Хост пересылает запросы `Ask`/`Stop`/`Status` расширению целиком: сериализует разобранный запрос с полем `type`, не перечисляя поля вручную.
  - Ответы сопоставляет по `id`. Для `stop`/`status` таймаут ожидания — 10 000 мс.
- [ ] **Step 4:** `cargo test --workspace` — PASS.
- [ ] **Step 5:** Commit `feat(protocol,host): chat target, stop and status requests, rate_limited code`.

### Задача 4: расширение — своя вкладка, чат по адресу, новый чат, стоп, rate limit

**Files:** `extension/src/bridge.ts`, `extension/src/content.ts`, `extension/src/chatgptAdapter.ts`, `extension/src/pageShim.ts`, `extension/src/bridgeProtocol.ts`, `extension/src/errors.ts`, `extension/src/targetTab.ts` (новый: выбор и удержание своей вкладки), `extension/manifest.json` (только если нужно разрешение `storage`)

**Interfaces — Consumes:** кадры задачи 3: `ask` с `target`, `stop`, `status`.

- [ ] **Step 1: Сверить разметку на живой странице** (только чтение, вкладка в группе Claude in Chrome). Записать в `research/` ответы на три вопроса:
  - селектор ссылки или кнопки «Новый чат»;
  - ссылки на чаты в боковой панели (`a[href^="/c/"]`);
  - текст или разметка сообщения об ограничении частоты, если его можно найти без провокации — иначе по HTTP 429 потока.
- [ ] **Step 2: Write the failing tests** (Node + esbuild, как в `research/`, без новых зависимостей; файл `extension/test/target.test.ts`, скрипт `npm run test`):
  - `targetTab.chooses_own_tab_for_targeted_ask`;
  - `targetTab.untargeted_ask_uses_legacy_active_tab_rules`;
  - `targetTab.reopens_own_window_when_closed`;
  - `adapter.conversationPathFrom` — для `/c/local-chatgpt:…` возвращает `null`, для `/c/<id>` — путь;
  - `shim.reports_http_429_as_rate_limited`.
- [ ] **Step 3:** Реализовать.
  - **Своя вкладка.** Запросы с `target` идут во вкладку, которую расширение открывает в отдельном окне без фокуса (`chrome.windows.create({ focused: false, url: "https://chatgpt.com/" })`). Её id хранится в `chrome.storage.session`, разрешение `storage` при необходимости.
  - **Перед отправкой** content script приводит вкладку к цели:
    - `new_chat` → нажать «Новый чат»;
    - `conversation` → если путь отличается, нажать ссылку из боковой панели, иначе `location.assign(url)`.
    - Дождаться готовности поля ввода.
  - **После ответа** вернуть `conversation_url` (настоящий путь, не временный).
  - **`stop`** — нажать «Стоп» в своей вкладке, если идёт генерация, вернуть `stopped { was_generating }`.
  - **`status`** — наличие своей вкладки, вход (есть поле ввода), текущий путь.
  - **Rate limit.** Шим при HTTP 429 у потока посылает `sse-error` с `status: 429`; адаптер превращает его и текст ограничения на странице в ошибку `rate_limited`.
- [ ] **Step 4:** `npm run typecheck && npm run test && npm run build` — PASS.
- [ ] **Step 5:** Commit `feat(extension): own tab, chat targeting, new chat, stop, status, rate_limited`.

### Задача 5: локальный сокет на Windows — named pipe

**Files:** `protocol/src/local_socket.rs` (вместо `socket_path.rs`), `protocol/Cargo.toml`, `host/src/main.rs`, `cli/src/transport.rs`

**Interfaces — Produces:**

```rust
pub fn default_endpoint() -> String;                                   // Unix: путь как сейчас; Windows: "cgpt-bridge-<user>" (имя pipe)
pub fn connect(endpoint: &str, timeout_ms: u64) -> io::Result<LocalStream>;   // interprocess::local_socket::Stream
pub fn listen(endpoint: &str) -> io::Result<LocalListener>;            // с правилом «живой занят → ошибка, мёртвый файл → удалить»
```

- [ ] **Step 1: Write the failing tests:**
  - `endpoint_unix_matches_old_socket_path` (`cfg(unix)`);
  - `endpoint_windows_is_pipe_name_with_user` (`cfg(windows)`);
  - `listen_connect_roundtrip_frame` — кадр туда и обратно через `read_frame`/`write_frame`;
  - `second_listener_refused_while_first_alive`;
  - `stale_unix_socket_file_replaced` (`cfg(unix)`).
- [ ] **Step 2:** Run → FAIL.
- [ ] **Step 3:** Реализовать на `interprocess`. Перевести хост и CLI на `connect`/`listen`. Таймауты чтения и записи на Windows — через отдельный поток с ожиданием, если нужно.
- [ ] **Step 4:**
  - `cargo test --workspace` — PASS;
  - `cargo check --workspace --target x86_64-pc-windows-msvc` — без ошибок: теперь компилируется и хост, и CLI.
- [ ] **Step 5:** Commit `feat: local socket via interprocess — Unix socket and Windows named pipe`.

### Задача 6: шлюз — каркас, авторизация, SSE, локальный заголовок

**Files:** `gateway/Cargo.toml`, `gateway/src/{main,config,http,openai,backend,title}.rs`, `Cargo.toml` (members)

**Interfaces — Produces:**

```rust
// openai.rs — serde-типы ChatRequest { model, messages: Vec<Message>, tools: Option<Vec<Tool>>, stream: Option<bool>, ... }
//   Message { role, content: Option<Content /* String | Vec<Part{type,text}> */>, tool_calls: Option<Vec<ToolCall>>, tool_call_id: Option<String> }
//   ToolCall { id, type: "function", function: { name, arguments: String } }; Tool { type, function: { name, description, parameters } }
pub enum Reply { Text(String), ToolCalls { content: String, calls: Vec<ToolCall> } }
// backend.rs
pub trait ChatBackend: Send { fn ask(&mut self, text: &str, target: &ChatTarget, timeout_ms: u64) -> Result<(String, Option<String>), BackendError>;
                              fn stop(&mut self) -> Result<(), BackendError>; }
pub enum BackendError { Unavailable(String), RateLimited, Timeout, Dom(String), Other(String) }
pub struct BridgeBackend { endpoint: String } // через protocol::local_socket
pub struct FakeBackend { pub replies: VecDeque<Result<(String, Option<String>), BackendError>>, pub sent: Vec<(String, ChatTarget)> }
// http.rs
pub fn serve(cfg: Config, backend: Box<dyn ChatBackend>, engine: Engine /* задача 7–9 подставляют логику */) -> io::Result<()>;
pub fn sse_reply(id: &str, model: &str, reply: &Reply, usage: Usage) -> Vec<String>; // строки "data: …\n\n"
// title.rs
pub fn local_title(req: &ChatRequest) -> String; // первые 6–8 слов первого <message>…</message> или первого user
```

- [ ] **Step 1: Write the failing tests:**
  - `config_created_with_random_token_and_defaults` — token 64 hex, порт 8765, `turn_timeout_ms` 900000, `allow_tools` пустой;
  - `binds_only_loopback`;
  - `missing_or_wrong_token_401`;
  - `models_lists_chatgpt_web_and_local_title`;
  - `local_title_answers_without_backend` — `FakeBackend.sent` пуст, ответ — первые слова сообщения;
  - `stream_reply_shape` — первая строка с `role: "assistant"`, последний кусок `finish_reason`, `usage`, `[DONE]`; для `ToolCalls` — `finish_reason: "tool_calls"` и `tool_calls[0].function.arguments` — строка JSON;
  - `non_stream_reply_shape`;
  - `keepalive_comment_every_10s` — при задержке backend 25 с (подменяемые часы или параметр интервала для теста) в потоке есть `: keepalive`;
  - `client_disconnect_calls_backend_stop`;
  - `backend_errors_map_to_status` — 503 / 429 / 504 / 502 с человеческим текстом;
  - `requests_are_serialized` — два параллельных запроса, `FakeBackend` видит их строго по одному;
  - `utf8_cyrillic_and_emoji_roundtrip` (Review Focus 5).
- [ ] **Step 2:** `cargo test -p cgpt-gateway`. Ожидается: FAIL.
- [ ] **Step 3:** Реализовать.
  - В этой задаче `Engine` — простой: последнее сообщение пользователя уходит как есть в `NewChat`, ответ — `Reply::Text`. Задачи 7–9 его заменяют.
  - Токен — через `getrandom`.
  - `main` читает конфиг из `dirs`-подобного `<data_dir>`: свою функцию по `APPDATA`/`HOME`, без новой зависимости.
- [ ] **Step 4:** PASS.
- [ ] **Step 5:** Commit `feat(gateway): OpenAI-compatible server skeleton with auth, SSE, keepalive and local titles`.

### Задача 7: перевод запроса и ответа, вызовы инструментов

**Files:** `gateway/src/prompt.rs`, `gateway/src/reply.rs`

**Interfaces — Produces:**

```rust
// prompt.rs
pub fn gateway_rules() -> &'static str;                                 // правила формата из спецификации 6.3
pub fn compact_tools(tools: &[Tool], allow: &[String]) -> String;       // имя, описание, компактная схема; execute выкинут, если не в allow
pub fn bootstrap(req: &ChatRequest, allow: &[String]) -> Vec<String>;   // части ≤ MAX_MESSAGE_CHARS, всего ≤ 2 частей по правилам сжатия
pub fn delta(msgs: &[Message], note_interrupted: bool, updated_rules: Option<(&str, &str)>) -> Vec<String>;
pub fn split(text: &str) -> Vec<String>;                                 // "часть N из M, ответь словом ok"
// reply.rs
pub enum ParseOutcome { Text(String), Calls { prefix: String, calls: Vec<(String, serde_json::Value)> }, Invalid(String /* что не так */) }
pub fn parse_reply(text: &str, tools: &[Tool]) -> ParseOutcome;
pub fn repair_request(problem: &str) -> String;
```

- [ ] **Step 1: Write the failing tests:**
  - `bootstrap_order_rules_system_tools_history` — сначала правила, затем системная подсказка, инструменты, история;
  - `execute_dropped_unless_allowed`;
  - `compact_schema_keeps_names_types_required_enums_only`;
  - `bootstrap_from_real_capture_fits_one_part` — запрос из `research/spike-opencode-gateway/captures`, скопированный в `gateway/tests/fixtures/opencode-first.json`;
  - `bootstrap_compresses_old_tool_results_to_2000_and_keeps_last_3`;
  - `bootstrap_drops_oldest_turns_with_note_when_over_two_parts`;
  - `delta_formats_tool_results_with_id_and_name`;
  - `delta_starts_with_interrupted_note_when_flagged`;
  - `delta_includes_updated_rules_block_when_given`;
  - `everything_redacted_before_split` — ключ `AKIA…` на границе 60 000 не попадает кусками;
  - `huge_tool_result_split_parts_within_limit` (Review Focus 4);
  - `parse_text_without_block`;
  - `parse_single_block_multiple_calls_with_prefix_text`;
  - `parse_ignores_other_code_blocks` (Review Focus 3);
  - `parse_two_tool_blocks_invalid`;
  - `parse_unknown_tool_invalid`;
  - `parse_missing_required_param_invalid`;
  - `parse_wrong_top_level_type_invalid`;
  - `parse_block_label_case_insensitive`.
- [ ] **Step 2:** Run → FAIL.
- [ ] **Step 3:** Реализовать. Правила формата для ChatGPT — текст из спецификации 6.3, по-русски. Напоминание в каждой дельте — одна строка.
- [ ] **Step 4:** PASS.
- [ ] **Step 5:** Commit `feat(gateway): request-to-chat translation and tool_calls reply parsing`.

### Задача 8: сессии и чаты

**Files:** `gateway/src/session.rs`; `gateway/src/http.rs` (Engine)

**Interfaces — Produces:**

```rust
pub enum SessionState { Idle, InFlight, Interrupted }
pub struct Session { pub key: String, pub chat_url: Option<String>, pub delivered: Vec<u64> /* отпечатки несистемных сообщений */,
                     pub system_hash: u64, pub tools_hash: u64, pub state: SessionState, pub last_used_ms: u128, pub call_seq: u32 }
pub struct Sessions { dir: PathBuf }
impl Sessions { pub fn key_for(headers: &[(String,String)], req: &ChatRequest) -> String; pub fn load(&self, key: &str) -> Option<Session>;
                pub fn save(&self, s: &Session) -> io::Result<()>; pub fn expire(&self, now_ms: u128) -> io::Result<usize>; }
pub enum Plan { Bootstrap /* новый чат */, Delta { from: usize, note_interrupted: bool, updated_rules: bool } }
pub fn plan_turn(s: Option<&Session>, req: &ChatRequest) -> Plan;
pub struct Engine { /* sessions, cooldown, safety, config */ }
impl Engine { pub fn handle(&mut self, headers: &[(String,String)], req: &ChatRequest, backend: &mut dyn ChatBackend) -> Result<Reply, BackendError>; }
```

- [ ] **Step 1: Write the failing tests** (`FakeBackend`, временная папка):
  - `first_request_bootstraps_new_chat_and_saves_url`;
  - `continuation_sends_only_delta_to_same_chat`;
  - `changed_system_prompt_sends_updated_rules_into_same_chat`;
  - `edited_history_starts_new_chat`;
  - `session_key_from_header_else_first_user_hash`;
  - `retry_after_interrupt_has_note_and_no_duplicate` (Review Focus 1);
  - `two_sessions_never_mix` — запросы A, B, A, B: каждая в своём чате, адреса не путаются (Review Focus 2);
  - `repair_once_then_text_to_user` — первый ответ с неверным блоком → отправлена просьба исправить, второй неверный → `Reply::Text` с объяснением;
  - `call_ids_unique_per_session`;
  - `sessions_persist_and_expire_after_7_days`.
- [ ] **Step 2:** Run → FAIL.
- [ ] **Step 3:** Реализовать.
  - Отпечаток — FNV-1a 64 от канонического JSON сообщения: `role`, `content`, `tool_calls` по имени и аргументам без id, `tool_call_id`.
  - На время хода — `InFlight`. Ошибка, таймаут или обрыв → `Interrupted`.
- [ ] **Step 4:** PASS.
- [ ] **Step 5:** Commit `feat(gateway): per-session chats with fingerprinted delta delivery`.

### Задача 9: безопасность и паузы в шлюзе

**Files:** `gateway/src/safety.rs`, `gateway/src/cooldown.rs`, `gateway/src/session.rs` (вызовы)

**Interfaces — Produces:**

```rust
pub enum CallCheck { Pass, Deny(String /* причина для ChatGPT */) }
pub fn check_call(name: &str, args: &serde_json::Value, tools: &[Tool]) -> CallCheck; // command → classify; маркер redact в любых строках аргументов
pub struct Cooldown { path: PathBuf } // как в спецификации: 5 / 15 / 30 мин
impl Cooldown { pub fn record_hit(&self, now_ms: u128) -> io::Result<u128>; pub fn active_until(&self, now_ms: u128) -> io::Result<Option<u128>>; }
```

- [ ] **Step 1: Write the failing tests:**
  - `blocked_command_not_returned_and_chatgpt_told_rule_name`;
  - `second_blocked_in_same_turn_becomes_text`;
  - `command_param_checked_for_any_tool_name` — инструмент `powershell` с `command`;
  - `redaction_marker_in_patch_args_rejected`;
  - `allowed_command_passes`;
  - `rate_limited_sets_cooldown_and_returns_429_with_time`;
  - `cooldown_5_15_30_within_hour_and_reset_after`;
  - `active_cooldown_refuses_without_backend_call`.
- [ ] **Step 2:** Run → FAIL.
- [ ] **Step 3:** Реализовать и встроить в `Engine::handle` после `parse_reply`.
- [ ] **Step 4:** PASS. `cargo test --workspace`, `cargo clippy --workspace` без новых предупреждений.
- [ ] **Step 5:** Commit `feat(gateway): denylist and redaction-marker guard on tool calls, rate-limit cooldown`.

### Задача 10: живая проверка на Mac (веха «держит ли ChatGPT формат»)

Выполняет контроллер. Нужны собранные хост, шлюз и расширение из этой ветки (расширение перезагружает пользователь), OpenCode 2.1.4.

- [ ] **Step 1:** Песочница `research/spike-opencode-gateway/sandbox` с `opencode.json`: провайдер `cgpt` с `chatgpt-web` и `local-title`, `small_model: cgpt/local-title`, токен из конфига шлюза. Скиллы через `.agents/skills` песочницы: копия `dev-process` из `~/dev/codex-kit`, глобальные папки не трогаются.
- [ ] **Step 2:** Сценарии `opencode run` и интерактивно, каждый ход — в журнал шлюза (разобран без просьбы / с просьбой / не разобран):
  1. прочитать и найти;
  2. правка через `patch`;
  3. команда `shell`;
  4. вопрос `question`;
  5. скилл `skill`;
  6. подагент `subagent`;
  7. «Стоп» посреди хода и повтор;
  8. две сессии параллельно;
  9. запрещённая команда — шлюз не отдаёт её OpenCode.
- [ ] **Step 3:** Порог из спецификации: из ≥20 ходов с инструментами ≥90% без просьбы исправить и ≥98% с ней. Не прошли — остановить план и показать цифры пользователю.
- [ ] **Step 4:** Подобрать реальный предел одного сообщения (начать с 60 000). Если отличается — поправить `MAX_MESSAGE_CHARS` отдельным коммитом. Отчёт — в `research/2026-10-…-gateway-live-check.md`.

### Задача 11: установка на Windows и macOS, CI

**Files:** `install/windows/install.ps1`, `install/windows/uninstall.ps1`, `install/windows/opencode-provider.json`, `install/macos/install.sh`, `install/macos/uninstall.sh`, `install/macos/com.cgpt-bridge.gateway.plist`, `.github/workflows/release.yml` (замена старого), `scripts/build-release.ps1`, `scripts/build-release.sh` (обновить: шлюз в архиве), `research/windows-setup.md` (переписать под шлюз), `README.md` (раздел установки для обеих ОС)

- [ ] **Step 1: Write the failing tests** (Pester не используем; проверки — в CI-шаге PowerShell на временном профиле):
  - установка в `%LOCALAPPDATA%\cgpt-bridge\` создаёт файлы, ключ `HKCU\Software\Google\Chrome\NativeMessagingHosts\com.cgpt_bridge.host`, конфиг шлюза с токеном, значение автозапуска `cgpt-gateway` в `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`;
  - в существующий `opencode.json` с другим провайдером добавлен `cgpt`, остальное не тронуто, рядом копия `.bak`;
  - повторный запуск ничего не дублирует;
  - `uninstall.ps1` убирает всё своё и не трогает чужое.
  - то же для macOS в CI-шаге bash на временном `HOME`: файлы в `~/Library/Application Support/cgpt-bridge/`, манифест хоста Chrome, конфиг шлюза с токеном, LaunchAgent-plist (без `launchctl` в CI — только файл и `plutil -lint`), правка `opencode.json` с `.bak`, идемпотентность, `uninstall.sh`.
- [ ] **Step 2:** Run in CI → FAIL.
- [ ] **Step 3:** Реализовать скрипты.
  - Workflow: матрица `windows-latest` и `macos-latest`. В каждой ОС: `cargo test --workspace`, `cargo build --release -p cgpt-bridge-host -p cgpt-gateway -p cgpt-bridge-cli`, сборка расширения, проверки установки. Артефакты: `cgpt-bridge-<версия>-windows-x64.zip` и `cgpt-bridge-<версия>-macos-<arch>.tar.gz`.
  - Запуск: на push в ветку и вручную.
- [ ] **Step 4:** CI зелёный.
- [ ] **Step 5:** Commit `feat(install): Windows and macOS installers, OpenCode provider setup and CI build`.

### Задача 12: приёмка на Windows и Mac

Пользователь с подсказками контроллера:
1. Ставит OpenCode и Chrome-расширение, распаковывает архив из CI, запускает `install.ps1`.
2. Открывает проект с копированием приложения, выбирает модель «ChatGPT Web».
3. Проходит один этап своего плана со скиллами codex-kit (критерий спецификации, раздел 2).

Отдельно — установка из архива CI на Mac пользователя (`install.sh`) и короткий прогон в OpenCode.

Замечания — в `research/` и новыми задачами.

---

## Что проверить в конце плана

- `cargo test --workspace` зелёный в CI на Windows и macOS.
- `cargo clippy --workspace` без новых предупреждений.
- CLI `cgpt ask` / `cgpt agent` работают как раньше: короткий прогон на живом мосте.
- Живая проверка (задача 10) пройдена по порогу. Отчёт в `research/`.
