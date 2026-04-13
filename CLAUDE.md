# Quay — Claude Code Project Guide

> Workspace nativo cross-platform onde múltiplos agentes IA de coding
> (Claude Code, OpenCode) operam em paralelo. Inspirado no Lanes.sh
> mas escrito em Rust + Slint, GPU-acelerado, ~60 FPS no Linux.

## ⚠️ REGRA ABSOLUTA — NUNCA use `--release` em desenvolvimento

**NÃO rode**, sob NENHUMA circunstância, os seguintes comandos durante
iteração de código:

- ❌ `cargo build --release`
- ❌ `cargo test --release`
- ❌ `cargo check --release`
- ❌ `cargo run --release`
- ❌ `cargo clippy --release`

Esses comandos levam **8–15 MINUTOS cada** porque o release profile
tem `lto = "thin" + codegen-units = 1`, que serializam todo o codegen
e forçam LLVM a re-otimizar o binário inteiro. Rodar em loop trava
o desenvolvimento.

**Use em vez disso:**

- ✅ `cargo build` — debug, rebuild incremental em ~6s
- ✅ `cargo check` — type-check, rebuild em ~3s
- ✅ `cargo nextest run --all-targets` (ou `cargo test --all-targets`)
- ✅ `cargo clippy --all-targets -- -D warnings`
- ✅ `./target/debug/quay` — smoke launch em debug funciona igualzinho

O binário debug renderiza exatamente a mesma UI que o release. A UI
não precisa de otimizações -O3 pra funcionar. **`--release` existe
exclusivamente para builds de distribuição** (CI release job, upload
de artifact, medição de performance final).

Se um teste depende de runtime performance (rare), use o profile
`release-fast` (definido no `Cargo.toml`): `cargo test --profile
release-fast`. Ele mantém `-O3` mas desliga LTO, então rebuild é
~2 min em vez de 15 min.

## TL;DR para o Claude

- **Stack**: Rust 2024 + Slint 1.15 (renderer Skia) + portable-pty +
  alacritty_terminal + git2 + rusqlite + syntect/ropey (editor).
- **Comunicação com o usuário**: **português brasileiro**. Termos
  técnicos e identificadores ficam em inglês.
- **Build do dia-a-dia**: `cargo build` (debug). Rebuild incremental
  em ~6s. Ver seção "Build / test workflow" abaixo.
- **Testes**: `cargo nextest run --all-targets` (preferido) ou
  `cargo test --all-targets`. NUNCA com `--release`.
- **Linker rápido no Linux**: o projeto usa `mold` via
  `.cargo/config.toml`. Instalação: `sudo apt install mold`.
- **Smoke launch**: `./target/debug/quay` (ou release após build
  final). Janela abre igual.
- **Cargo.lock está commitado** (é um binário, não uma lib).
- **Cold build do zero**: ~10 min inescapáveis — Skia (C++ 200+ MB)
  + libgit2 + SQLite precisam compilar uma vez. Depois cache.

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
├── settings.rs          ← Settings KV wrapper
└── i18n.rs              ← locale detection + switching (rust-i18n + Slint gettext)

ui/main.slint            ← UI declarativa (~3700 linhas)

locales/                 ← rust-i18n YAML (strings Rust: toasts, menu)
├── en.yml               ← inglês (fallback default)
└── pt-BR.yml            ← português brasileiro

i18n/                    ← gettext (strings Slint: @tr() labels, botões)
├── quay.pot             ← template (gerado por slint-tr-extractor)
└── pt-BR/LC_MESSAGES/
    └── quay.po          ← tradução pt-BR (bundled no build via build.rs)

scripts/
└── i18n-update.sh       ← automação: extract → merge → compile traduções
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

**IMPORTANTE — escolhendo o profile certo:** este projeto tem 3 profiles
de build, cada um com propósito distinto. Usar o errado desperdiça
minutos ou horas de compilação.

```bash
# 1. DEV (cargo build padrão) — o caminho default pra iteração.
#    Rebuild incremental em ~6s. Use pra smoke launch, quase sempre.
cargo build                          # debug, ~6s incremental
timeout 3 ./target/debug/quay 2>&1   # smoke launch em debug (funciona igual)

# 2. RELEASE-FAST — quando você precisa do binário otimizado (teste de
#    performance, reproducao de bug runtime-only) mas não quer o LTO.
#    Rebuild em ~1-2 min, binário roda perto da velocidade release.
cargo build --profile release-fast
./target/release-fast/quay

# 3. RELEASE — APENAS para builds de distribuição (CI release job,
#    upload de artifact, benchmark final). Leva 8-15 min porque usa
#    `lto = "thin" + codegen-units = 1`. NÃO USE PRA ITERAÇÃO.
cargo build --release                # 8-15 min, use raramente

# Testes — nextest paraleliza e é o comando preferido
cargo nextest run --all-targets      # ~6s + <1s execução

# Lints
cargo clippy --all-targets -- -D warnings
```

**Regra geral:** se você está iterando código, use `cargo build` (debug)
ou `cargo build --profile release-fast`. **Nunca use `cargo build --release`
em loop de desenvolvimento** — é exclusivamente pra builds de distribuição.

**Multi-worktree (agentes em paralelo):** cada worktree tem seu próprio
`target/`, então cold build é pago por worktree. Mitigação: `sccache`
está configurado globalmente (`rustc-wrapper` em `~/.cargo/config.toml`),
então artifacts do rustc (Skia, libgit2, SQLite) são compartilhados via
content-addressable cache. Segundo worktree de uma branch é quase instant.

### Performance do build

Rebuild incremental típico (após `touch src/main.rs`): **~6s**. Cache
hit total (sem mudanças): **<1s**. Cold build do zero: ~10 min (o peso
é Skia C++ + libgit2 + SQLite, não o rustc).

O `Cargo.toml` usa três truques para acelerar dev/test builds:

1. **`debug = "line-tables-only"`** em `profile.dev` e `profile.test` —
   reduz o test binary de 498 MB → 158 MB e corta ~45% do tempo de
   rebuild incremental porque gerar DWARF completo é o gargalo real
   aqui. Backtraces continuam mostrando file:line.
2. **`[profile.dev.package."*"] opt-level = 3`** — compila as
   dependências (Skia, libgit2, SQLite, syntect) em -O3 mesmo em dev
   build. User code fica em `opt-level = 0` para recompilar rápido,
   mas os testes que batem nas deps rodam perto da velocidade de
   release.
3. **`.cargo/config.toml` com mold** — linker rápido no Linux (~10%
   adicional). Sem impacto no runtime.

O primeiro `cargo build` após alterar qualquer profile vai ser lento
(~4-10 min) porque invalida o cache de deps. Iterações subsequentes
usam o cache.

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

## Internacionalização (i18n)

O Quay suporta múltiplos idiomas. Atualmente: **inglês** (default) e
**português brasileiro**. A arquitetura usa **dois sistemas** em
paralelo, cada um nativo ao seu domínio:

| Domínio | Ferramenta | Formato | Macro |
|---------|-----------|---------|-------|
| Strings Rust (toasts, menu sidebar) | `rust-i18n` | YAML (`locales/*.yml`) | `t!("key")` |
| Strings Slint (labels, botões, headers) | Slint gettext | `.po` (`i18n/*/LC_MESSAGES/`) | `@tr("text")` |

### Como funciona

1. **Startup**: `i18n::init_locale()` lê a preferência do usuário
   (SQLite `settings.locale`) ou detecta o locale do sistema via
   `sys-locale`. Chama `rust_i18n::set_locale()` + 
   `slint::select_bundled_translation()`.

2. **Runtime**: o usuário troca o idioma na página Settings. O callback
   `on_locale_changed` chama `i18n::apply_locale()` que atualiza ambos
   os backends. As strings `@tr()` do Slint re-renderizam
   automaticamente; as strings `t!()` do menu sidebar são reconstruídas
   via `rebuild_menu_model()`.

3. **Build**: `build.rs` usa `.with_bundled_translations("i18n")` para
   compilar os `.po` no binário. `slint-tr-extractor` extrai strings
   `@tr()` para o `.pot`.

### Adicionando um novo idioma

1. Criar `locales/<tag>.yml` (ex: `locales/es.yml`) com as mesmas
   chaves de `locales/en.yml`, sem nível de locale como root — o locale
   é derivado do nome do arquivo.
2. Copiar `i18n/pt-BR/` → `i18n/<tag>/LC_MESSAGES/quay.po` e traduzir
   os `msgstr`.
3. Adicionar o locale em `src/i18n.rs:SUPPORTED_LOCALES`.
4. Adicionar um botão no selector de idioma em
   `ui/components/settings_page.slint` (seção Language).
5. Rodar `cargo build` — o build.rs bundla o `.po` automaticamente.

### Adicionando uma nova string traduzível

- **Lado Rust** (toast, menu): adicionar a chave em `locales/en.yml` e
  `locales/pt-BR.yml`, usar `t!("chave").to_string()` no código.
- **Lado Slint** (UI label): usar `@tr("texto em inglês")` no `.slint`,
  depois rodar `scripts/i18n-update.sh` para atualizar o `.pot` e
  mergear no `.po`.

### Formato dos YAML (`locales/*.yml`)

**IMPORTANTE**: as chaves são flat com pontos como separador, **sem**
nível de locale como raiz. O locale é derivado do nome do arquivo.

```yaml
# locales/en.yml  ← correto
menu.new_cli_session: "New CLI Session"
tasks.created: "Created '%{title}'"
```

```yaml
# ERRADO — NÃO usar locale como raiz
en:
  menu:
    new_cli_session: "New CLI Session"
```

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

## Estado atual (2026-04-12)

- **39 polishes** commitados além das fases originais (Spike A/B/C +
  Fases 1-7 + Tasks 1-9). A UI está visualmente próxima da referência
  Lanes (~95% de match), com extras (Cmd+P task switcher, toast
  notifications, sidebar collapse, animations everywhere).
- **i18n**: suporte a múltiplos idiomas (en + pt-BR). ~124 strings
  `@tr()` em 16 arquivos Slint + ~46 strings `t!()` em YAML. Troca de
  idioma em runtime via página Settings. Deps: `rust-i18n`, `sys-locale`.
- **Settings como página**: Settings foi convertido de modal overlay
  para página inline (`active-page: "home" | "settings"`).
- **159 testes** unit + integration, todos verdes. Cobrem o data layer
  (kanban store, git wrappers, claude_stats, file_tree, settings,
  quick_actions, process, schema migrations, i18n locale resolution).
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
| Tradução mostra chave raw | `locales/*.yml` — chaves devem ser flat sem locale prefix; verificar `RUST_I18N_DEBUG=1 cargo check` |
| @tr() não traduz | Verificar se `.po` existe em `i18n/<locale>/LC_MESSAGES/quay.po` e `build.rs` tem `with_bundled_translations` |
| Idioma não muda | `src/i18n.rs` — verificar `resolve_locale()` e `SUPPORTED_LOCALES`; menu sidebar usa `rebuild_menu_model()` |

## Referências externas

- Slint docs: https://docs.slint.dev/latest/docs/slint/
- portable-pty: https://docs.rs/portable-pty/latest/portable_pty/
- alacritty_terminal: https://docs.rs/alacritty_terminal/0.26.0/alacritty_terminal/
- syntect: https://docs.rs/syntect/latest/syntect/
- git2: https://docs.rs/git2/latest/git2/
- Lanes.sh (referência visual original): https://lanes.sh
