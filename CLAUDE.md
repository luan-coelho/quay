# Quay — Claude Code Project Guide

> Workspace nativo cross-platform onde múltiplos agentes IA de coding
> (Claude Code, OpenCode) operam em paralelo. Inspirado no Lanes.sh
> mas escrito em Rust + Slint, GPU-acelerado, ~60 FPS no Linux.

## TL;DR para o Claude

- **Stack**: Rust 2024 + Slint 1.15 (renderer Skia) + portable-pty +
  alacritty_terminal + git2 + rusqlite + syntect/ropey (editor).
- **Comunicação com o usuário**: **português brasileiro**. Termos
  técnicos e identificadores ficam em inglês.
- **Build**: `cargo build --release`. **Toma 8–12 minutos** por causa
  de `lto = "thin" + codegen-units = 1` no profile release.
- **Testes**: `cargo test --release` — 101 testes, todos verdes.
  Mesmo problema de tempo de compilação.
- **Smoke launch**: `timeout 3 ./target/release/quay 2>&1 | tail -5`
  — verifica que a janela abre e o glyph atlas é construído.
- **Cargo.lock está commitado** (é um binário, não uma lib).

## Por que Quay existe

Foi construído para substituir o Lanes.sh (macOS-only Electron app)
por uma alternativa nativa, cross-platform e visualmente fiel mas
performática. O fluxo principal é:

1. Você cria uma task no kanban (`Cmd+N`).
2. Clica em **Plan** ou **Implement** — Quay cria um worktree git
   isolado com branch slug determinístico (`5-brave-otter`), spawna
   um agente IA dentro dele com seu prompt como argumento, e move
   a task para a coluna apropriada.
3. Múltiplas sessões rodam em paralelo, cada uma em sua própria PTY +
   worktree. O usuário alterna entre elas pela strip de open tabs ou
   clicando nos cards do kanban.
4. Ao terminar, mover a task para Done remove o worktree (se clean).

## Layout do projeto

```
src/
├── main.rs              ← entry point + Slint wiring (~2000 linhas)
├── app.rs               ← AppState (sessions, open_tabs, expanded_dirs)
├── agents/              ← Strategy pattern para CLIs IA
│   ├── mod.rs           ← trait AgentProvider + factory detect()
│   ├── claude_code.rs   ← ClaudeCodeProvider impl
│   ├── opencode.rs      ← OpencodeProvider impl
│   ├── claude_resume.rs ← captura session id de ~/.claude/projects
│   └── claude_stats.rs  ← parse JSONL para tokens/cost/runtime
├── kanban/              ← model + stores (Task, Label, Dependency, Project)
│   ├── model.rs         ← Task struct + enums (TaskState, AgentKind, etc.)
│   ├── store.rs         ← TaskStore + label/dep/project stores
│   ├── labels.rs        ← LabelStore com 13 presets Lanes
│   ├── deps.rs          ← DependencyStore com cycle check via CTE
│   └── projects.rs      ← ProjectStore com get_or_create_for_repo
├── git/                 ← worktree + status + diff + naming
│   ├── worktree.rs      ← WorktreeManager (shell-out git)
│   ├── status.rs        ← read_status via git2
│   ├── diff.rs          ← read_diff + read_commit_log
│   └── naming.rs        ← branch_slug determinístico
├── persistence/
│   └── schema.rs        ← migrations v1 → v7 + run_migrations()
├── terminal/
│   ├── session.rs       ← PtySession (portable-pty wrapper)
│   ├── render.rs        ← GlyphAtlas + blitter
│   └── framebuffer.rs   ← Framebuffer (RGBA8)
├── editor/              ← Phase 7 in-app editor
│   ├── buffer.rs        ← EditorBuffer (ropey + syntect)
│   └── highlight.rs     ← syntect → HighlightedLineData
├── file_tree.rs         ← build_tree + open_in_editor
├── process.rs           ← sysinfo enumerate/terminate/kill
├── quick_actions.rs     ← QuickActionStore CRUD
└── settings.rs          ← Settings KV wrapper

ui/main.slint            ← UI declarativa (~3700 linhas)
```

## Arquitetura essencial

### `AppState` (`src/app.rs`)

Estado global da janela. Owns:
- `sessions: RefCell<HashMap<Uuid, PtySession>>` — uma PTY por task
- `active_task: RefCell<Option<Uuid>>` — qual task está visível no right pane
- `open_tabs: RefCell<Vec<Uuid>>` — pinned no right-pane tab strip (persistido em settings)
- `expanded_dirs: RefCell<HashSet<PathBuf>>` — diretórios expandidos no Files tab
- `framebuffer: RefCell<Framebuffer>` — buffer RGBA8 que recebe blits do GlyphAtlas
- `db: Database` — handle SQLite compartilhado entre todos os stores

Métodos críticos:
- `start_session(id, mode)` — cria worktree (se needed), resolve agent
  argv via Strategy, spawna PtySession, persiste session_state
- `select_task(id)` — alterna `active_task` (sem spawn)
- `pin_open_tab(id)` / `close_open_tab(id)` — gerencia open tabs
- `cleanup_worktree_on_done(id)` — chamado quando task move para Done

### Strategy pattern dos agentes

`trait AgentProvider` em `src/agents/mod.rs` expõe:
- `name() -> &'static str` — "claude" / "opencode"
- `argv(mode, instructions, resume_id) -> Vec<String>`
- `env() -> Vec<(String, String)>`
- `supports_resume() -> bool`

A factory `detect(AgentKind) -> Result<Option<Box<dyn AgentProvider>>>`
retorna `None` para `AgentKind::Bare` (que bypassa o Strategy e usa
`$SHELL` direto). Adicionar Cursor/Aider/Gemini é criar um arquivo
novo em `src/agents/`, implementar a trait, e adicionar 1 branch em
`detect()`.

### Schema SQLite (migrations v1 → v7)

Migrações vivem em `src/persistence/schema.rs:MIGRATIONS`.
**Nunca editar uma migration já lançada** — sempre adicionar uma nova.
SQLite não suporta `ALTER TABLE … DROP CONSTRAINT`, então mudanças em
CHECK constraints exigem table swap (ver migration v3).

Tabelas:
- `tasks` — id, title, description, instructions, state, kind,
  cli_selection, start_mode, worktree_strategy, session_state,
  process_pid, claude_session_id, project_id, position, created_at,
  updated_at, repo_path, branch_name, worktree_path
- `sessions` — log de sessões PTY (session_id, task_id, started_at,
  ended_at, status)
- `labels`, `task_labels` — Phase 4 (Polish 3, 13 presets seedados)
- `task_dependencies` — Phase 4 com cycle check via recursive CTE
- `projects` — Phase 6
- `quick_actions` — Phase 5 (atalhos Cmd+Alt+1..9)
- `settings` — KV simples (key TEXT PK, value TEXT)

### Slint ↔ Rust bridge

`slint::include_modules!()` em `main.rs` gera structs Rust para os
`export struct …` declarados em `ui/main.slint`. O wiring fica em
`main.rs` via `window.on_X(callback)` + `window.set_X(value)`.

Modelos longos (kanban columns, file tree, open tabs, search results)
usam `Rc<VecModel<...>>` que é passado para Slint via `ModelRc::from()`.
Mutações no modelo refletem reativamente na UI.

### Padrões de UI já estabelecidos

Componentes reutilizáveis em `ui/main.slint` (procurar `^component`):
- **CardRow** — task row no kanban
- **OpenTaskTab** — chip no right-pane tab strip
- **MenuRow** / **ProjectRow** — sidebar entries
- **FilterChip** — top filter strip
- **CloseButton** — × button reusável (5 sites)
- **Avatar** — círculo decorativo no title bar
- **Spinner** — braille glyph cycling para tasks busy
- **Toast** — notificação transient com fade+slide
- **ShortcutRow** — chip + label do Cmd+? overlay
- **KindDiamond** / **KindChip** — micro-componentes para kind tags
- **SectionHeader** — header dos kanban columns

Se um padrão visual aparece **2+ vezes open-coded**, extraia em
componente. Se aparece 1 vez, não.

## Build / test workflow

```bash
# Build (lento — 8 a 12 min em release)
cargo build --release

# Tests (101 verdes — 8-12 min na primeira vez, ~30s subsequente)
cargo test --release

# Smoke launch (verifica que a janela abre)
timeout 3 ./target/release/quay 2>&1 | tail -5

# Lints
cargo clippy --release -- -D warnings
```

## Convenções de código

- **Não criar arquivos novos** se uma função pode caber em um arquivo
  existente. Quay valoriza `src/main.rs` denso (~2000 linhas) ao invés
  de explosão de módulos pequenos.
- **Comentários `// Polish N:`** indicam de qual polish veio uma seção.
  Útil para git blame mental — não remover sem motivo.
- **Animações Slint** seguem `120ms ease-out` por padrão. Sites:
  CardRow background, OpenTaskTab background, modais (160-200ms).
- **Spinner é window-wide**: `MainWindow.spinner-glyph: string` cycled
  pelo `poll_timer` em main.rs cada ~96ms (6 ticks de 16ms). Todos os
  spinners (CardRow + OpenTaskTab) leem a mesma propriedade — todos
  em lockstep, exatamente como cargo/npm/rustup spinners funcionam.
- **Toast helper** (`show_toast: Rc<dyn Fn(&str, String)>`) é clonável
  em closures. Sempre usar pra erros visíveis ao usuário; tracing-only
  errors são para debug.
- **Não animar propriedades derivadas** — Slint 1.15 só anima bindings
  concretos. `sidebar-w` foi tornado property derivada (não animada);
  para animar precisaria de state machine.
- **Slint não tem `rotation-angle` em Rectangle/Image** — esse foi
  o motivo do spinner ser braille cycle ao invés de SVG arc.

## Coisas a NÃO fazer

- ❌ Não rodar `cargo build` direto sem `--release` — o profile dev
  funciona mas o binário fica lento e o tempo total é igual.
- ❌ Não amend commits — sempre criar commits novos (instrução do
  usuário, evita perder trabalho em hook failures).
- ❌ Não usar `git add .` ou `git add -A` — sempre adicionar arquivos
  por nome para evitar incluir cruft.
- ❌ Não rodar `cargo update` sem motivo — Cargo.lock é fonte de
  verdade pra reproducibility.
- ❌ Não fazer push pra origin sem instrução explícita do usuário.
- ❌ Não inserir colunas em tabelas existentes via `ALTER TABLE`
  quando a coluna tem `CHECK` — SQLite vai rejeitar. Use migration
  com table swap.
- ❌ Não criar componente Slint novo se há ≤1 ocorrência open-coded.
  YAGNI.
- ❌ Não tocar `tests/` ou `examples/` sem motivo — são spikes
  históricos.

## Estado atual (2026-04-11)

- **39 polishes** commitados além das fases originais (Spike A/B/C +
  Fases 1-7 + Tasks 1-9). A UI está visualmente próxima da referência
  Lanes (~95% de match), com extras (Cmd+P task switcher, toast
  notifications, sidebar collapse, animations everywhere).
- **101 testes** unit + integration, todos verdes. Cobrem o data layer
  (kanban store, git wrappers, claude_stats, file_tree, settings,
  quick_actions, process, schema migrations).
- **Cargo.lock** commitado. Rust edition 2024.
- **CI** (GitHub Actions matrix Linux/Win/macOS) está estruturado mas
  não validado num push real ainda — task #9 pendente.

## Onde olhar primeiro quando algo quebra

| Sintoma | Olhar em |
|---|---|
| Slint não compila | `cargo build` mostra linha. Procurar em `ui/main.slint` (~3700 linhas) |
| Rust compile error | Geralmente em `src/main.rs` (callbacks longos) |
| Runtime crash no startup | `tracing` logs no stderr; `quay starting` é a 1ª linha |
| Terminal não renderiza | `terminal/session.rs` (PTY read loop) ou `terminal/render.rs` (blit) |
| Worktree não cria | `git/worktree.rs` shell-out — checar permissões e existência da branch base |
| Migration falha | `persistence/schema.rs` — não editar migrations já lançadas |
| Spinner não anima | `main.rs` poll_timer — verificar `set_spinner_glyph` no closure |
| Toast não aparece | `main.rs` `show_toast` clone — confirmar que a closure capturou |

## Referências externas

- Slint docs: https://docs.slint.dev/latest/docs/slint/
- portable-pty: https://docs.rs/portable-pty/latest/portable_pty/
- alacritty_terminal: https://docs.rs/alacritty_terminal/0.26.0/alacritty_terminal/
- syntect: https://docs.rs/syntect/latest/syntect/
- git2: https://docs.rs/git2/latest/git2/
- Lanes.sh (referência visual original): https://lanes.sh
