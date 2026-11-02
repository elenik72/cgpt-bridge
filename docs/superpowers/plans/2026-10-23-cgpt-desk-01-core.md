# cgpt-desk, план 1: ядро агента — план реализации

> **Заменён (superseded) 26.10.2026.** Этот план не выполняется. Его заменил план шлюза для OpenCode `docs/superpowers/plans/2026-10-26-cgpt-gateway.md`. Из него в работу вошёл только крейт `core/` с denylist и вычисткой секретов.

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Крейт `core` с полным циклом агента по протоколу v2 (инструменты, разрешения, запуск команд, фоновые процессы, сессии, ввод в курс дела) и консольная команда `cgpt desk`, которая проводит работу через живой мост ещё до графического интерфейса.

**Architecture:** Новый крейт `core/` (`cgpt-bridge-core`) — библиотека без UI: цикл агента общается с ChatGPT через трейт `ChatTransport` и с человеком через трейт `UserGate`. Поэтому одно и то же ядро потом подключится к Tauri (план 3), а сейчас — к консоли (`cgpt desk`) и к поддельным реализациям в тестах. Протокол v2 живёт в `protocol/`. denylist и вычистка секретов переезжают из `cli/` в `core/`, CLI использует их оттуда без изменения поведения.

**Tech Stack:** Rust (workspace, edition 2021), serde/serde_json, regex, `ignore` (обход папок с учётом `.gitignore`), `dunce` (пути Windows без `\\?\`), `dirs` (папка данных приложения), `windows-sys` (Job Object, только Windows).

**Spec:** `docs/superpowers/specs/2026-10-23-windows-agent-app-design.md`

**Место в серии:** план 1 из 4.
- План 2 — мост: named pipe, своя вкладка, адресация чата, «Стоп», `rate_limited`.
- План 3 — приложение на Tauri.
- План 4 — Windows: установщик, CI, приёмка.

План 2 от этого плана не зависит, его можно делать параллельно.

## Global Constraints

- Ответ ChatGPT — ровно один fenced-блок с меткой `cgpt-agent-v2`. Ответ приложения — блок `cgpt-result-v2`. Метку сравнивать без учёта регистра.
- В одном ходе: до 8 действий чтения (`list_dir`, `read_file`, `search`, `read_skill`, `process_output`) и не больше одного изменяющего (`edit_file`, `write_file`, `run_command`, `start_process`, `stop_process`). Изменяющее выполняется после чтений.
- `ask_user` — единственное действие хода и только со `status: "ask"`. `status: "final"` — без действий.
- Пределы:
  - `read_file` — окно до 600 строк и до 30 000 символов;
  - вывод любого другого действия — до 20 000 символов;
  - одно сообщение в ChatGPT — до 60 000 символов (константа `MAX_MESSAGE_CHARS`, подбирается в плане 2);
  - `search` — до 200 совпадений;
  - `AGENTS.md` в сумме — до 32 768 байт.
- `timeout_ms` команды:
  - по умолчанию 120 000;
  - больше 600 000 — только с подтверждением, даже в режиме «Без подтверждений»;
  - потолок 1 800 000, выше — урезается с предупреждением.
- Фоновые процессы: не больше 3 на сессию, буфер вывода — последние 2 000 строк, ожидание `ready_pattern` — не дольше 60 с.
- При нарушении протокола — одна автоматическая просьба исправить за ход.
- Паузы при ограничении частоты запросов: 5 мин, повтор в течение часа — 15, затем 30.
- Граница записи — открытая папка проекта. Запись в `.git/` запрещена всегда. Чтение вне проекта — только папки скиллов и найденные `AGENTS.md`.
- Сначала вычистка секретов, потом обрезка. Действует для любого текста, который уходит в ChatGPT.
- Режимы: `Normal`, `AcceptEdits`, `Plan`, `Bypass` — таблица 7.5 спецификации.
- Данные приложения: `<data_dir>/cgpt-desk/`, на Windows это `%APPDATA%\cgpt-desk\`. В проект ничего не пишется.
- Поведение CLI `cgpt ask` и `cgpt agent` не меняется, их тесты остаются зелёными.
- Новые зависимости — только перечисленные в Tech Stack.

## Review Focus

1. **Не-UTF-8 и бинарные файлы.** Например, UTF-16 с BOM — обычное дело на Windows — или картинка. `read_file` не падает: UTF-16 декодирует, для бинарного возвращает «двоичный файл, N байт». Тест в задаче 4.
2. **Переводы строк CRLF.** ChatGPT присылает `old` с `\n`, а файл в `\r\n`. `edit_file` находит фрагмент и сохраняет переводы строк файла. Тест в задаче 4.
3. **Пути от ChatGPT.** Абсолютные Windows-пути внутри проекта, пробелы и кириллица — принимаются. `..`, ссылки наружу и другой диск — отклоняются понятной ошибкой. Тест в задаче 3.
4. **Метка блока.** Блок с меткой в другом регистре (`CGPT-AGENT-V2`) или окружённый прозой — принимается. Два блока — ошибка `DuplicateBlocks`. Тест в задаче 2.
5. **Огромный или бесконечный вывод команды** (логи `npm install` на мегабайты). Память ограничена, в ChatGPT уходит не больше 20 000 символов. Тест в задаче 7.

---

## Файлы

```
core/                         новый крейт cgpt-bridge-core
  Cargo.toml
  src/lib.rs                  модули ниже
  src/denylist.rs             перенесён из cli/ + правила PowerShell/cmd (задачи 1, 6)
  src/redact.rs               перенесён из cli/ (задача 1)
  src/limits.rs               числовые пределы из Global Constraints (задача 2)
  src/workspace.rs            граница проекта и разрешение путей (задача 3)
  src/tools/mod.rs            ToolOutput, общий тип результата инструмента
  src/tools/fs.rs             list_dir, read_file, write_file, edit_file (задача 4)
  src/tools/search.rs         search (задача 5)
  src/exec.rs                 оболочка ОС, запуск команды, убийство дерева (задача 7)
  src/procs.rs                фоновые процессы (задача 8)
  src/context.rs              AGENTS.md, скиллы, пакет ввода в курс дела (задача 9)
  src/permissions.rs          режимы, правила «всегда», решение по действию (задача 10)
  src/store.rs                проекты, сессии, журнал, исходники для отката (задача 11)
  src/cooldown.rs             паузы после rate limit (задача 12)
  src/agent.rs                цикл агента, трейты ChatTransport/UserGate (задача 13)
  src/bridge_transport.rs     ChatTransport поверх текущего хоста (задача 14)
protocol/src/agent_v2.rs      типы, разбор и проверка протокола v2, форматирование результатов (задача 2)
protocol/src/agent_prompt_contract_v2.md   текст правил протокола для ChatGPT (задача 9)
cli/src/desk.rs               команда `cgpt desk` (задача 14)
```

---

### Задача 1: крейт `core`, перенос denylist и redact

**Files:**
- Create: `core/Cargo.toml`, `core/src/lib.rs`
- Move: `cli/src/denylist.rs` → `core/src/denylist.rs`, `cli/src/redact.rs` → `core/src/redact.rs` (`git mv`)
- Modify: `Cargo.toml` (workspace members), `cli/Cargo.toml`, `cli/src/lib.rs`

**Interfaces:**
- Produces:
  - `cgpt_bridge_core::denylist::{classify, Classification, LocalRisk, RuleHit}`;
  - `cgpt_bridge_core::redact::{redact, truncate, Redaction, TruncateOutcome}`.

  Сигнатуры не меняются.

- [ ] **Step 1:** Создать крейт `cgpt-bridge-core`.
  - Зависимости: `cgpt-bridge-protocol`, serde, serde_json, regex (из workspace).
  - Добавить `"core"` в `members`.
  - Перенести оба файла `git mv`.
  - В `cli/src/lib.rs` заменить `pub mod denylist; pub mod redact;` на `pub use cgpt_bridge_core::{denylist, redact};`. Так пути `crate::denylist` в `runner.rs` продолжают работать.
- [ ] **Step 2:** Запустить `cargo test --workspace`. Ожидается: все тесты зелёные, включая перенесённые тесты denylist и redact (их число не изменилось — 102 на 23.10.2026).
- [ ] **Step 3:** Commit `refactor: extract denylist and redact into cgpt-bridge-core`.

### Задача 2: протокол v2 в `protocol/`

**Files:**
- Create: `protocol/src/agent_v2.rs`, `core/src/limits.rs`
- Modify: `protocol/src/lib.rs` (`pub mod agent_v2;`), `protocol/src/agent.rs` (сделать `extract_fenced_bodies` `pub(crate)` и сравнивать метку без учёта регистра)

**Interfaces:**
- Produces (в `cgpt_bridge_protocol::agent_v2`):

```rust
pub const AGENT_FENCE_V2: &str = "cgpt-agent-v2";
pub const RESULT_FENCE_V2: &str = "cgpt-result-v2";
pub enum TurnStatus { Continue, Ask, Final }                 // serde: "continue"|"ask"|"final"
pub struct PlanItem { pub id: String, pub text: String, pub status: PlanItemStatus } // pending|doing|done
pub struct AgentTurnV2 { pub version: u32, pub status: TurnStatus, pub message: String,
                         pub plan: Option<Vec<PlanItem>>, pub actions: Vec<Action> }
pub enum Action {  // #[serde(tag = "type", rename_all = "snake_case")], у каждого поле id: String
  ListDir { id, path: String, depth: Option<u32> },
  ReadFile { id, path: String, offset: Option<u64>, limit: Option<u64> },
  Search { id, pattern: String, path: Option<String>, glob: Option<String>, regex: Option<bool> },
  ReadSkill { id, name: String },
  ProcessOutput { id, name: String, tail: Option<u32> },
  EditFile { id, path: String, edits: Vec<Edit> },           // Edit { old, new, replace_all: Option<bool> }
  WriteFile { id, path: String, content: String, overwrite: Option<bool> },
  RunCommand { id, command: String, cwd: Option<String>, timeout_ms: Option<u64>, risk: Option<String>, description: Option<String> },
  StartProcess { id, name: String, command: String, cwd: Option<String>, ready_pattern: Option<String>, description: Option<String> },
  StopProcess { id, name: String },
  AskUser { id, question: String, options: Vec<String>, allow_free_text: Option<bool> },
  UpdatePlan { id },
}
impl Action { pub fn id(&self) -> &str; pub fn is_read(&self) -> bool; pub fn is_mutating(&self) -> bool; }
pub enum ParseErrorV2 { NoBlock, DuplicateBlocks { count: usize }, BadJson { detail: String },
                        BadVersion { found: serde_json::Value }, SchemaInvalid { detail: String },
                        RuleViolation { detail: String } }
pub struct ParsedTurn { pub turn: AgentTurnV2, pub warnings: Vec<String> }
pub fn parse_turn_v2(text: &str) -> Result<ParsedTurn, ParseErrorV2>;
pub enum ResultStatus { Ok, Error, Rejected }
pub struct ActionResult { pub id: String, pub status: ResultStatus, pub body: String }
pub fn format_results_v2(results: &[ActionResult], user_note: Option<&str>) -> String;
pub fn repair_prompt_v2(err: &ParseErrorV2) -> String;
```

- Produces (в `cgpt_bridge_core::limits`): `READ_WINDOW_LINES = 600`, `READ_WINDOW_CHARS = 30_000`, `ACTION_OUTPUT_CHARS = 20_000`, `MAX_MESSAGE_CHARS = 60_000`, `SEARCH_MAX_MATCHES = 200`, `MAX_READS_PER_TURN = 8`, `DEFAULT_TIMEOUT_MS = 120_000`, `CONFIRM_ABOVE_TIMEOUT_MS = 600_000`, `MAX_TIMEOUT_MS = 1_800_000`, `MAX_BG_PROCESSES = 3`, `BG_BUFFER_LINES = 2_000`, `READY_WAIT_MS = 60_000`, `INSTRUCTIONS_MAX_BYTES = 32_768`.

- [ ] **Step 1: Write the failing tests** (в `agent_v2.rs`, `mod tests`):

```rust
#[test] fn parses_minimal_final() { /* status final, actions [] → Ok, turn.status == Final */ }
#[test] fn fence_label_is_case_insensitive() { /* "```CGPT-AGENT-V2\n{...}\n```" среди прозы → Ok */ }
#[test] fn two_blocks_rejected() { assert!(matches!(parse_turn_v2(two), Err(ParseErrorV2::DuplicateBlocks{count:2}))); }
#[test] fn more_than_eight_reads_rejected() { /* 9 read_file → RuleViolation */ }
#[test] fn two_mutating_rejected() { /* edit_file + run_command → RuleViolation */ }
#[test] fn ask_user_must_be_alone_with_status_ask() { /* ask_user + read_file → RuleViolation; ask_user со status continue → RuleViolation */ }
#[test] fn final_must_have_no_actions() { /* final + read_file → RuleViolation */ }
#[test] fn timeout_above_max_clamped_with_warning() { /* timeout_ms 2_000_000 → 1_800_000, warnings.len()==1 */ }
#[test] fn duplicate_action_ids_rejected() { /* два действия с id "a1" → RuleViolation */ }
#[test] fn format_results_has_result_fence_and_ids() { /* содержит "```cgpt-result-v2", "\"id\": \"a1\"", "\"status\": \"rejected\"" */ }
```

- [ ] **Step 2:** `cargo test -p cgpt-bridge-protocol agent_v2`. Ожидается: FAIL (модуль не существует).
- [ ] **Step 3:** Реализовать типы и функции выше.
  - `version` — только 2, иначе `BadVersion`.
  - Неизвестные поля верхнего уровня — `SchemaInvalid` (`deny_unknown_fields`).
  - Тело результата — JSON `{"version":2,"results":[{id,status,body}],"user_note":…}` внутри fence `cgpt-result-v2`, в конце строка-напоминание «Ответь одним блоком cgpt-agent-v2».
  - `repair_prompt_v2` говорит, что именно не так, и повторяет правило одного блока.
  - Пределы как константы — в `core/src/limits.rs`. Протокол берёт свои 8, 600 000 и 1 800 000 из собственных `const` с теми же значениями: `protocol` не зависит от `core`.
- [ ] **Step 4:** `cargo test -p cgpt-bridge-protocol`. Ожидается: PASS, тесты v1 не сломаны.
- [ ] **Step 5:** Commit `feat(protocol): agent protocol v2 parser and result formatter`.

### Задача 3: граница проекта

**Files:** Create `core/src/workspace.rs` (+ `dunce` в `core/Cargo.toml`)

**Interfaces:**
- Produces:

```rust
pub struct Workspace { root: PathBuf /* canonical, dunce */, read_roots: Vec<PathBuf> }
pub enum PathError { OutsideProject(String), GitDirWrite(String), NotFound(String), Io(String) }
impl Workspace {
  pub fn new(root: &Path, read_roots: Vec<PathBuf>) -> io::Result<Self>;
  pub fn root(&self) -> &Path;
  pub fn resolve_read(&self, p: &str) -> Result<PathBuf, PathError>;   // внутри root или одного из read_roots
  pub fn resolve_write(&self, p: &str) -> Result<PathBuf, PathError>;  // только внутри root, не .git/, файл может не существовать
  pub fn resolve_cwd(&self, p: Option<&str>) -> Result<PathBuf, PathError>; // существующая папка внутри root, None → root
  pub fn display(&self, abs: &Path) -> String; // путь относительно root с '/' для ChatGPT
}
```

- [ ] **Step 1: Write the failing tests:**
  - `relative_inside_ok`;
  - `absolute_inside_ok` — абсолютный путь внутри временного root;
  - `dotdot_escape_rejected` — `../x` → `OutsideProject`;
  - `symlink_escape_rejected` (`#[cfg(unix)]`);
  - `git_dir_write_rejected` — `.git/config` → `GitDirWrite`;
  - `write_new_file_in_new_subdir_ok` — `src/new/a.ts`, папки нет;
  - `read_root_allows_skill_dir`;
  - `read_root_does_not_allow_write`;
  - `cyrillic_and_spaces_ok` — `"папка с пробелом/файл.md"`;
  - `case_insensitive_on_windows` (`#[cfg(windows)]`).
- [ ] **Step 2:** `cargo test -p cgpt-bridge-core workspace`. Ожидается: FAIL.
- [ ] **Step 3:** Реализовать.
  - Для несуществующего пути канонизировать ближайшего существующего предка и приклеить остаток, предварительно отклонив компоненты `..`.
  - Сравнение `starts_with` по канонизированным путям, на Windows — по lowercase.
- [ ] **Step 4:** Тесты зелёные.
- [ ] **Step 5:** Commit `feat(core): project boundary and path resolution`.

### Задача 4: файловые инструменты

**Files:** Create `core/src/tools/mod.rs`, `core/src/tools/fs.rs` (+ `ignore` в зависимостях)

**Interfaces:**
- Consumes: `Workspace` (задача 3), `redact::redact` (задача 1), `limits`.
- Produces:

```rust
pub struct ToolOutput { pub ok: bool, pub body: String }       // body уже вычищен и обрезан до ACTION_OUTPUT_CHARS
pub struct FileChange { pub path: PathBuf, pub before: Option<Vec<u8>>, pub after: Vec<u8> }
pub fn list_dir(ws: &Workspace, path: &str, depth: u32) -> ToolOutput;            // depth по умолчанию 2, .gitignore учитывается
pub fn read_file(ws: &Workspace, path: &str, offset: u64, limit: u64) -> ToolOutput; // offset с 1
pub fn write_file(ws: &Workspace, path: &str, content: &str, overwrite: bool) -> Result<(ToolOutput, FileChange), ToolOutput>;
pub fn edit_file(ws: &Workspace, path: &str, edits: &[Edit]) -> Result<(ToolOutput, FileChange), ToolOutput>;
```

  `Edit` — тип из `agent_v2`.

- [ ] **Step 1: Write the failing tests:**
  - `read_window_has_line_numbers_and_next_offset` — файл 1 000 строк, окно 1..600, тело кончается подсказкой `offset=601`;
  - `read_window_char_cap` — 600 строк по 100 символов, тело ≤ 30 000 символов и подсказка, откуда читать;
  - `read_utf16_bom_decoded`;
  - `read_binary_reports_size` — тело содержит «двоичный файл» и размер;
  - `read_redacts_secrets` — строка `AKIA…`-ключа заменена;
  - `edit_unique_match_applied`;
  - `edit_no_match_error_lists_hint` — ok=false, тело говорит «не найдено»;
  - `edit_multiple_matches_error_without_replace_all` — тело содержит число совпадений;
  - `edit_replace_all_ok`;
  - `edit_matches_lf_old_against_crlf_file_and_keeps_crlf`;
  - `write_refuses_overwrite_without_flag`;
  - `write_creates_dirs`;
  - `list_dir_respects_gitignore`.
- [ ] **Step 2:** `cargo test -p cgpt-bridge-core tools::fs`. Ожидается: FAIL.
- [ ] **Step 3:** Реализовать функции выше.
  - Бинарность: NUL-байт в первых 8 КиБ без BOM.
  - CRLF: если файл содержит `\r\n`, нормализовать `old` и `new` к `\r\n` перед поиском.
  - Каждое тело ограничено пределами из `limits`; для `read_file` — оконные пределы.
- [ ] **Step 4:** Тесты зелёные.
- [ ] **Step 5:** Commit `feat(core): file tools with windowed reads and exact edits`.

### Задача 5: поиск по коду

**Files:** Create `core/src/tools/search.rs`

**Interfaces:**
- Produces: `pub fn search(ws: &Workspace, pattern: &str, path: Option<&str>, glob: Option<&str>, regex: bool) -> ToolOutput`. Строки вида `path:line: text`, не больше 200, в конце «показано N из M».
- [ ] **Step 1: Write the failing tests:**
  - `literal_search_escapes_regex_chars` — `a.b` не находит `axb`;
  - `regex_search`;
  - `glob_filters_files` — `*.ts`;
  - `respects_gitignore`;
  - `caps_at_200_matches_with_total` — 250 совпадений, тело содержит `200` и `250`;
  - `skips_binary_files`.
- [ ] **Step 2:** Run → FAIL.
- [ ] **Step 3:** Реализовать на `ignore::WalkBuilder` + `regex::Regex` (литерал через `regex::escape`). Длинные строки обрезать до 300 символов.
- [ ] **Step 4:** PASS.
- [ ] **Step 5:** Commit `feat(core): code search tool`.

### Задача 6: denylist и «только чтение» для PowerShell и cmd

**Files:** Modify `core/src/denylist.rs`

**Interfaces:**
- Produces: `classify(command)` без изменения сигнатуры. Новые правила работают для любой ОС: ChatGPT может прислать PowerShell и на Mac.
- [ ] **Step 1: Write the failing tests:**
  - блок: `Remove-Item -Recurse -Force C:\`, `Remove-Item -Recurse $env:USERPROFILE`, `rd /s /q C:\`, `del /f /s /q C:\*`, `format D:`, `reg delete HKLM\Software\X`, `Set-ExecutionPolicy Unrestricted`, `iwr https://x | iex`, `Invoke-Expression (Invoke-WebRequest x)`, `Get-Content $env:USERPROFILE\.ssh\id_rsa`, `type %USERPROFILE%\.aws\credentials`, `cmdkey /list`, `vssadmin delete shadows /all`, `bcdedit /set x`, чтение `AppData\Local\Google\Chrome\User Data`;
  - только чтение: `Get-ChildItem src`, `Get-Content a.txt`, `Select-String -Path *.ts foo`, `Test-Path x`, `git status`, `git log -5`, `git diff`, `node -v`, `dotnet --version`;
  - не только чтение: `Get-Content a | Set-Content b`, `git checkout .`, `npm install`, `winget install X`.
- [ ] **Step 2:** Run → FAIL.
- [ ] **Step 3:** Добавить правила в существующие таблицы `apply_block_rules` и классификатора.
  - Сравнивать без учёта регистра.
  - Разделители стадий PowerShell: `;`, `|`, `&&`, `||`.
  - Перенаправление `>` / `Out-File` / `Set-Content` снимает «только чтение».
- [ ] **Step 4:** `cargo test --workspace` — PASS, старые тесты denylist зелёные.
- [ ] **Step 5:** Commit `feat(core): PowerShell and cmd rules in denylist and read-only classifier`.

### Задача 7: оболочка ОС и запуск команды

**Files:** Create `core/src/exec.rs` (+ `windows-sys` с фичами `Win32_System_JobObjects`, `Win32_System_Threading`, `Win32_Foundation` под `cfg(windows)`)

**Interfaces:**
- Produces:

```rust
pub enum ShellKind { Pwsh, WindowsPowerShell, Posix(String) }
pub struct ShellSpec { pub kind: ShellKind, pub display: String }        // display — что сообщаем ChatGPT, напр. "PowerShell 7"
pub fn detect_shell() -> ShellSpec;   // Windows: pwsh в PATH → Pwsh, иначе WindowsPowerShell; Unix: $SHELL или /bin/sh
pub struct ExecOutcome { pub exit_code: Option<i32>, pub stdout: String, pub stderr: String,
                         pub duration_ms: u64, pub timed_out: bool, pub cancelled: bool, pub output_truncated: bool }
pub fn run_command(shell: &ShellSpec, command: &str, cwd: &Path, timeout_ms: u64, cancel: &AtomicBool) -> io::Result<ExecOutcome>;
pub fn command_result_body(outcome: &ExecOutcome) -> String; // вычистка → обрезка до ACTION_OUTPUT_CHARS, код, время
```

- [ ] **Step 1: Write the failing tests** (Unix-варианты; Windows-аналоги под `#[cfg(windows)]` пишутся в этой же задаче, гоняются в CI плана 4):
  - `runs_and_captures_exit_code` — `exit 3` → `Some(3)`;
  - `timeout_kills_whole_tree` — `sleep 30 & echo $! > pid; wait`, timeout 500 мс → `timed_out`, процесс из `pid` не жив;
  - `cancel_flag_stops_command`;
  - `huge_output_bounded` — `yes | head -c 50000000`: память ограничена (буфер не больше 4 МиБ на поток), `output_truncated == true`, тело ≤ 20 000 символов;
  - `stdin_is_closed` — `read x; echo got:$x` завершается сразу с пустым `x`;
  - `body_redacts_before_truncating`.
- [ ] **Step 2:** Run → FAIL.
- [ ] **Step 3:** Реализовать.
  - Unix: `CommandExt::process_group(0)`, по таймауту или отмене — `kill(-pgid, SIGTERM)`, через 500 мс `SIGKILL` (как `kill_with_grace` в `cli/src/runner.rs`, но для группы).
  - Windows:
    - процесс создаётся suspended, добавляется в Job Object с `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` и возобновляется;
    - завершение — `TerminateJobObject`;
    - команда оборачивается префиксом `[Console]::OutputEncoding=[Text.Encoding]::UTF8; $OutputEncoding=[Text.Encoding]::UTF8;`;
    - флаги `-NoLogo -NoProfile -NonInteractive -Command`.
  - Захват stdout и stderr — двумя потоками с кольцевым ограничением.
- [ ] **Step 4:** PASS на macOS.
- [ ] **Step 5:** Commit `feat(core): cross-platform command runner with process-tree kill`.

### Задача 8: фоновые процессы

**Files:** Create `core/src/procs.rs`

**Interfaces:**
- Consumes: `ShellSpec`, механизм группы и Job Object из задачи 7 (вынести общий `spawn_in_group(...) -> GroupChild` в `exec.rs`).
- Produces:

```rust
pub struct ProcessManager { /* name → BgProcess, лимит MAX_BG_PROCESSES */ }
pub enum ProcState { Running, Exited(Option<i32>) }
pub struct ProcInfo { pub name: String, pub command: String, pub state: ProcState, pub started_at_ms: u128 }
impl ProcessManager {
  pub fn new(shell: ShellSpec) -> Self;
  pub fn start(&mut self, name: &str, command: &str, cwd: &Path, ready_pattern: Option<&str>) -> ToolOutput; // первые строки или строка с ready_pattern, ≤ READY_WAIT_MS
  pub fn output(&self, name: &str, tail: u32) -> ToolOutput;
  pub fn stop(&mut self, name: &str) -> ToolOutput;
  pub fn stop_all(&mut self);
  pub fn list(&self) -> Vec<ProcInfo>;
}
impl Drop for ProcessManager { /* stop_all */ }
```

- [ ] **Step 1: Write the failing tests:**
  - `start_waits_for_ready_pattern` — скрипт печатает `listening on 3000` через 300 мс;
  - `ready_timeout_still_running` — паттерн не появился; при `READY_WAIT_MS`, переопределённом в тесте на 500 мс, тело говорит «не дождались», состояние `Running`;
  - `fourth_process_rejected`;
  - `duplicate_name_rejected`;
  - `output_tail_last_lines` — буфер держит не больше 2 000 строк;
  - `stop_kills_tree`;
  - `drop_stops_all`;
  - `exited_process_reports_code`.
- [ ] **Step 2:** Run → FAIL.
- [ ] **Step 3:** Реализовать. Для теста ожидание вынести в поле `ready_wait_ms` с конструктором `with_ready_wait`.
- [ ] **Step 4:** PASS.
- [ ] **Step 5:** Commit `feat(core): session-scoped background processes`.

### Задача 9: инструкции, скиллы и пакет ввода в курс дела

**Files:**
- Create: `core/src/context.rs`, `protocol/src/agent_prompt_contract_v2.md`
- Modify: `protocol/src/agent_v2.rs` (`pub const PROMPT_CONTRACT_V2: &str = include_str!("agent_prompt_contract_v2.md");`)

**Interfaces:**
- Produces:

```rust
pub struct InstructionFile { pub path: PathBuf, pub content: String }
pub struct SkillInfo { pub name: String, pub description: String, pub dir: PathBuf, pub implicit_allowed: bool }
pub struct ContextSources { pub codex_home: PathBuf, pub home: PathBuf, pub extra_skill_dirs: Vec<PathBuf> }
impl ContextSources { pub fn from_env() -> Self; } // CODEX_HOME или ~/.codex
pub fn discover_instructions(project: &Path, src: &ContextSources) -> (Vec<InstructionFile>, Option<String> /* предупреждение об обрезке */);
pub fn discover_skills(project: &Path, src: &ContextSources) -> Vec<SkillInfo>;
pub fn skill_read_roots(skills: &[SkillInfo], instructions: &[InstructionFile]) -> Vec<PathBuf>; // для Workspace::read_roots
pub fn read_skill(skills: &[SkillInfo], name: &str) -> ToolOutput; // SKILL.md + список файлов папки
pub struct EnvInfo { pub os: String, pub shell: String, pub project_root: String, pub mode: String, pub date: String }
pub fn build_bootstrap(env: &EnvInfo, instr: &[InstructionFile], skills: &[SkillInfo], task: &str) -> Vec<String>; // 1 или 2 сообщения ≤ MAX_MESSAGE_CHARS
```

- [ ] **Step 1: Write the failing tests** (временные папки, `ContextSources` подставляется):
  - `global_override_wins` — есть оба файла в codex_home, берётся `AGENTS.override.md`;
  - `project_chain_root_to_cwd` — git-корень `/r` (с `.git`), проект `/r/a/b`, `AGENTS.md` в `/r` и `/r/a/b`, порядок `/r` → `/r/a/b`;
  - `one_file_per_dir_override_first`;
  - `empty_files_skipped`;
  - `cap_32k_with_warning`;
  - `no_git_root_only_project_dir`;
  - `skills_from_all_scopes` — `.agents/skills` проекта и предков до корня, `~/.agents/skills`, `~/.codex/skills`, extra;
  - `frontmatter_quoted_and_folded_description` — `description: "a: b"` и `description: >` с двумя строками;
  - `skill_without_name_skipped`;
  - `openai_yaml_disables_implicit` — `agents/openai.yaml` с `allow_implicit_invocation: false`;
  - `read_skill_unknown_name_error`;
  - `bootstrap_contains_contract_env_instructions_skills_task_in_order`;
  - `bootstrap_splits_when_over_limit` — задача уходит вторым сообщением.
- [ ] **Step 2:** Run → FAIL.
- [ ] **Step 3:** Реализовать.
  - Frontmatter разбирать собственным минимальным разбором, без YAML-зависимости: строки `key: value`, кавычки `"…"`/`'…'`, блоки `>`/`|` с отступом.
  - Текст `agent_prompt_contract_v2.md` — правила из раздела 8 спецификации:
    - формат блока, типы действий с полями;
    - пределы хода;
    - «вопросы — только через ask_user»;
    - «не используй Canvas»;
    - «читай скилл через read_skill, когда он подходит по описанию»;
    - «команды пишутся для указанной оболочки».
- [ ] **Step 4:** PASS.
- [ ] **Step 5:** Commit `feat(core): Codex-compatible instructions and skills discovery, bootstrap message`.

### Задача 10: режимы и разрешения

**Files:** Create `core/src/permissions.rs`

**Interfaces:**
- Consumes: `Action`, `classify`, `limits`.
- Produces:

```rust
pub enum Mode { Normal, AcceptEdits, Plan, Bypass }      // serde: "normal"|"accept_edits"|"plan"|"bypass"
pub struct Rules { pub command_prefixes: Vec<String> }    // serde, rules.json
pub enum Verdict { Auto, Ask { reason: String }, Deny { reason: String } }
pub fn decide(mode: Mode, action: &Action, rules: &Rules, skill_dirs: &[PathBuf]) -> Verdict;
pub fn rule_prefix_for(command: &str) -> String; // что запомнить по «Разрешить всегда»: первые 2 слова, напр. "npm test"
```

- [ ] **Step 1: Write the failing tests** — табличный тест `decision_table`.

  Чтение во всех режимах:

  | Режим | Действие | Ожидается |
  |---|---|---|
  | все | `read_file` | Auto |

  Правки:

  | Режим | Действие | Ожидается |
  |---|---|---|
  | Normal | `edit_file` | Ask |
  | AcceptEdits | `edit_file` | Auto |
  | Plan | `edit_file` | Deny |
  | Bypass | `edit_file` | Auto |

  Команды:

  | Режим | Действие | Ожидается |
  |---|---|---|
  | Normal | `run_command "git status"` | Auto |
  | Normal | `run_command "npm test"` | Ask |
  | Normal | `npm test` с правилом `npm test` | Auto |
  | Plan | `npm test` | Deny |
  | Plan | `git log` | Auto |
  | Bypass | `npm test` | Auto |
  | Bypass | `timeout_ms 900000` | Ask |
  | любой | команда из denylist | Deny |

  Фоновые процессы:

  | Режим | Действие | Ожидается |
  |---|---|---|
  | Normal | `start_process` | Ask |
  | Plan | `start_process` | Deny |
  | Bypass | `start_process` | Auto |

  Отдельные тесты:
  - `skill_script_needs_confirmation_in_normal` — команда содержит путь к папке скилла → Ask даже при совпадении с правилом;
  - `rule_prefix_two_words` — `npm test -- --watch` → `npm test`.
- [ ] **Step 2:** Run → FAIL.
- [ ] **Step 3:** Реализовать.
  - `ask_user`, `update_plan`, `process_output` → Auto.
  - `stop_process` → Auto.
- [ ] **Step 4:** PASS.
- [ ] **Step 5:** Commit `feat(core): permission modes and always-allow rules`.

### Задача 11: хранилище проектов и сессий

**Files:** Create `core/src/store.rs` (+ `dirs`)

**Interfaces:**
- Produces:

```rust
pub struct AppStore { base: PathBuf }                       // <data_dir>/cgpt-desk
impl AppStore {
  pub fn open_default() -> io::Result<Self>; pub fn open_at(base: &Path) -> io::Result<Self>;
  pub fn add_project(&self, root: &Path) -> io::Result<ProjectRec>; pub fn projects(&self) -> io::Result<Vec<ProjectRec>>;
  pub fn rules(&self, project_id: &str) -> io::Result<Rules>; pub fn save_rules(&self, project_id: &str, r: &Rules) -> io::Result<()>;
  pub fn new_session(&self, project_id: &str, title: &str, mode: Mode) -> io::Result<SessionStore>;
  pub fn open_session(&self, project_id: &str, session_id: &str) -> io::Result<SessionStore>;
  pub fn sessions(&self, project_id: &str) -> io::Result<Vec<SessionMeta>>;
}
pub struct ProjectRec { pub id: String, pub root: PathBuf, pub name: String }
pub struct SessionMeta { pub id: String, pub title: String, pub mode: Mode, pub chats: Vec<String> /* адреса чатов, последний — текущий */,
                         pub status: String /* idle|running|waiting|stopped|final */, pub created_ms: u128, pub updated_ms: u128 }
pub enum Event { /* serde tag "kind" */ UserMessage{text}, AssistantMessage{text}, Plan{items}, ActionProposed{action}, ActionDecision{id, verdict, by_user: bool},
                 ActionResult{id, ok, summary}, ChatAttached{url}, Error{message}, ModeChanged{mode} }
pub struct SessionStore { /* dir */ }
impl SessionStore {
  pub fn meta(&self) -> io::Result<SessionMeta>; pub fn update_meta(&self, f: impl FnOnce(&mut SessionMeta)) -> io::Result<()>;
  pub fn append(&self, ev: &Event) -> io::Result<()>; pub fn events(&self) -> io::Result<Vec<Event>>; // битые строки пропускаются
  pub fn record_change(&self, ch: &FileChange) -> io::Result<()>; // исходник сохраняется один раз на файл
  pub fn changed_files(&self) -> io::Result<Vec<ChangedFile>>;   // { path, original: Option<Vec<u8>>, last_written_hash }
  pub fn rollback(&self, path: &Path, force: bool) -> io::Result<RollbackOutcome>; // Restored | Deleted | ExternallyModified
}
```

- [ ] **Step 1: Write the failing tests** (`open_at(tempdir)`):
  - `project_roundtrip`;
  - `rules_roundtrip`;
  - `session_meta_and_events_roundtrip`;
  - `corrupt_event_line_skipped` — дописать мусорную строку, `events()` возвращает остальные;
  - `first_original_kept_on_second_edit`;
  - `rollback_restores_original`;
  - `rollback_deletes_file_created_in_session`;
  - `rollback_refuses_when_externally_modified_unless_forced`;
  - `sessions_sorted_by_updated_desc`;
  - `event_text_is_stored_redacted` — сообщение с секретом сохраняется уже вычищенным.
- [ ] **Step 2:** Run → FAIL.
- [ ] **Step 3:** Реализовать.
  - Раскладка: `projects.json`, `projects/<id>/rules.json`, `projects/<id>/sessions/<sid>/{meta.json,events.jsonl,originals/}`.
  - id проекта — первые 12 hex от FNV-1a канонического пути. Своя функция, без зависимостей.
  - Запись `meta.json` атомарная: временный файл + rename.
  - Хэш последней записи — тот же FNV-1a по байтам.
- [ ] **Step 4:** PASS.
- [ ] **Step 5:** Commit `feat(core): app data store for projects, sessions, events and rollback`.

### Задача 12: паузы после ограничения частоты

**Files:** Create `core/src/cooldown.rs`

**Interfaces:**
- Produces:

```rust
pub struct Cooldown { path: PathBuf }   // <base>/throttle.json: [hit_ms...], until_ms
impl Cooldown { pub fn at(base: &Path) -> Self;
  pub fn record_hit(&self, now_ms: u128) -> io::Result<u128 /* until_ms */>; // 5 мин; 2-е попадание за час — 15; 3-е и далее — 30
  pub fn active_until(&self, now_ms: u128) -> io::Result<Option<u128>>; }
```

- [ ] **Step 1: Write the failing tests:**
  - `first_hit_5_minutes`;
  - `second_within_hour_15`;
  - `third_within_hour_30`;
  - `hit_after_an_hour_resets_to_5`;
  - `inactive_after_until`.
- [ ] **Step 2:** Run → FAIL.
- [ ] **Step 3:** Реализовать.
- [ ] **Step 4:** PASS.
- [ ] **Step 5:** Commit `feat(core): rate-limit cooldown ledger`.

### Задача 13: цикл агента

**Files:** Create `core/src/agent.rs`

**Interfaces:**
- Consumes: всё выше.
- Produces:

```rust
pub enum ChatTarget { NewChat, Conversation(String) }
pub struct ChatReply { pub text: String, pub conversation_url: Option<String> }
pub enum TransportError { Unavailable(String), RateLimited, Timeout, DomFailure(String), Other(String) }
pub trait ChatTransport { fn send(&mut self, text: &str, target: &ChatTarget, timeout_ms: u64) -> Result<ChatReply, TransportError>; }

pub struct ApprovalRequest { pub action: Action, pub reason: String, pub suggested_rule: Option<String> }
pub enum UserDecision { Allow, AllowAlways, Reject { note: Option<String> } }
pub enum UserAnswer { Choice(String), Text(String) }
pub trait UserGate {
  fn approve(&mut self, req: &ApprovalRequest) -> UserDecision;
  fn answer(&mut self, question: &str, options: &[String], allow_free_text: bool) -> UserAnswer;
  fn event(&mut self, ev: &Event);                         // для отображения: всё, что пишется в журнал
}
pub struct AgentConfig { pub turn_timeout_ms: u64, pub max_turns: u32 /* 0 = без предела */ }
pub enum RunEnd { Final(String), Stopped, Error(String), CoolingDown { until_ms: u128 } }
pub struct Agent { /* ws, skills, shell, procs, session, rules, mode, cooldown, cancel: Arc<AtomicBool> */ }
impl Agent {
  pub fn new(project: &ProjectRec, session: SessionStore, store: &AppStore, sources: &ContextSources, mode: Mode) -> io::Result<Self>;
  pub fn cancel_handle(&self) -> Arc<AtomicBool>;
  pub fn set_mode(&mut self, mode: Mode);
  pub fn run(&mut self, user_text: &str, t: &mut dyn ChatTransport, g: &mut dyn UserGate, cfg: &AgentConfig) -> RunEnd;
  pub fn handoff(&mut self, t: &mut dyn ChatTransport, g: &mut dyn UserGate, cfg: &AgentConfig) -> RunEnd; // 7.7 спецификации
}
```

- [ ] **Step 1: Write the failing tests** (поддельные `ScriptedTransport` — очередь ответов + запись отправленного — и `ScriptedGate`):
  - `first_message_is_bootstrap_then_target_new_chat`;
  - `conversation_url_from_first_reply_attached_and_reused` — второй `send` идёт в `Conversation(url)`, в meta записан чат;
  - `reads_executed_then_results_sent_in_result_fence`;
  - `mutating_runs_after_reads`;
  - `normal_mode_asks_before_edit_and_reject_note_reaches_chatgpt`;
  - `allow_always_saves_rule_and_skips_next_prompt`;
  - `plan_mode_denied_edit_reported_as_rejected`;
  - `protocol_violation_gets_one_repair_then_stops_with_error`;
  - `ask_user_answer_sent_back` — ответ попадает в `user_note` результата;
  - `final_ends_run_with_message`;
  - `cancel_flag_stops_between_actions` — прерывает запущенную команду и останавливает фоновые процессы;
  - `rate_limited_records_cooldown_and_returns_cooling_down`;
  - `active_cooldown_refuses_to_send`;
  - `edit_records_original_for_rollback`;
  - `handoff_asks_summary_then_new_chat_with_bootstrap_and_summary`;
  - `command_changed_files_detected_via_git_status` — в git-песочнице команда `echo x > a.txt` даёт событие `CommandChangedFiles { id, files: ["a.txt"] }`; без git события нет;
  - `oversized_results_split_into_messages` — результаты больше `MAX_MESSAGE_CHARS` уходят несколькими сообщениями, каждое не длиннее предела; промежуточные помечены «часть N из M, ответь одним словом "ок"», и ответы на них не разбираются как ход.

  Добавить `CommandChangedFiles { id: String, files: Vec<String> }` в `Event` (задача 11). Снимок — `git status --porcelain=v1 -z` до и после команды; если папка не в git, снимка нет.
- [ ] **Step 2:** Run → FAIL.
- [ ] **Step 3:** Реализовать цикл.
  1. Выполняется только при неактивной паузе.
  2. Первое сообщение — `build_bootstrap`, цель `NewChat`. Иначе — текст пользователя, цель `Conversation(последний чат)`.
  3. Разбор через `parse_turn_v2`. При ошибке — одна отправка `repair_prompt_v2`, повторная ошибка → `RunEnd::Error`.
  4. `plan` → событие `Plan`.
  5. Чтения выполняются по очереди, затем изменяющее с `decide` → `approve`.
  6. `ask_user` → `answer`.
  7. Результаты → `format_results_v2` → `send`.
  8. Пока не `final`, не `cancel` и не превышен `max_turns`.

  Все тексты для журнала проходят `redact`. `Workspace::read_roots` = `skill_read_roots`.
- [ ] **Step 4:** PASS.
- [ ] **Step 5:** Commit `feat(core): agent loop over ChatTransport and UserGate`.

### Задача 14: транспорт к текущему хосту и команда `cgpt desk`

**Files:**
- Create: `core/src/bridge_transport.rs`, `cli/src/desk.rs`
- Modify: `cli/src/args.rs` (подкоманда `desk`), `cli/src/main.rs`, `README.md` (раздел «cgpt desk»)

**Interfaces:**
- Consumes: `cli::transport::ask_once` — перенести в `core::bridge_transport` вместе с `new_request_id`; CLI реэкспортирует.
- Produces:
  - `pub struct BridgeTransport { socket: PathBuf }`, `impl ChatTransport for BridgeTransport`. Пока хост не умеет адресовать чат (это план 2), `target` игнорируется, отправка идёт по правилам расширения (липкая вкладка), `conversation_url` = `None`.
  - Ошибки хоста отображаются в `TransportError`: `Timeout` → `Timeout`, `DomFailure` → `DomFailure`, `TabUnavailable` или `ExtensionUnavailable` → `Unavailable`.
  - `cgpt desk [--project <dir>] [--mode normal|accept-edits|plan|bypass] [--resume <session>] [TASK]` — консольный `UserGate`:
    - события печатаются построчно;
    - подтверждение — клавиши `y` / `a` / `n` + заметка;
    - вопрос — номер варианта или текст.
- [ ] **Step 1: Write the failing tests:**
  - `bridge_transport_maps_errors` — на поддельном Unix-сокете по образцу `cli/tests/ask_smoke.rs`;
  - `desk_args_parse_modes`;
  - `console_gate_parses_decisions` — `y` / `a` / `n причина`.
- [ ] **Step 2:** Run → FAIL.
- [ ] **Step 3:** Реализовать.
- [ ] **Step 4:** `cargo test --workspace` — PASS.
- [ ] **Step 5: Живая проверка на Mac** (мост установлен, вкладка ChatGPT открыта):
  1. Взять временную папку-песочницу и скопировать в `~/.agents/skills` скилл `dev-process` из `~/dev/codex-kit/codex-kit/`. Если скилл уже есть — ничего не трогать, а использовать `extra_skill_dirs` через переменную `CGPT_DESK_EXTRA_SKILLS`.
  2. Запустить `cargo run -p cgpt-bridge-cli -- desk --project <песочница> "Создай маленькое консольное приложение на Node, которое печатает дату; запусти его; спроси меня, верен ли вывод"`.

  Ожидается:
  - ChatGPT читает файлы;
  - создание файла — после `y`;
  - `node` запускается;
  - приходит вопрос;
  - после ответа — `final`;
  - сессия видна в `<data_dir>/cgpt-desk/`.

  Отдельно проверить `start_process` на `node -e "require('http').createServer(()=>{}).listen(3000,()=>console.log('listening'))"` с `ready_pattern: listening`, затем `stop_process`.
- [ ] **Step 6:** Commit `feat(cli): cgpt desk — headless runner for the v2 agent core`.

---

## Что проверить в конце плана

- `cargo test --workspace` зелёный на macOS.
- `cargo clippy --workspace` без новых предупреждений.
- `cgpt ask` и `cgpt agent` работают как раньше: короткий ручной прогон на живом мосте.
- Живая проверка из задачи 14 пройдена. Отчёт о ней — в описании последнего коммита или в `research/`.
