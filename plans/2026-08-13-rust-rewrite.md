# NextSync Rust Rewrite — Implementation Plan

> **Decisiones (13-Ago-2026, usuario A FUEGO):** refactor a Rust aprobado. Se abandona la caza de bugs de la línea Python (el bug de Settings de ryzen-ai queda sin diagnosticar). Todo el conocimiento de git se usa para el rewrite: issues #4/#5/#7/#8/#9/#12/#13/#16-#22, `docs/REDESIGN.md`, `docs/REFACTOR-NOTES.md`, CHANGELOG, arquitectura v0.2.5 y spike validado en `/tmp/opencode/spike-ncsync`.

**Goal:** Reescribir NextSync (wrapper fino sobre `nextcloudcmd`) en Rust + gtk-rs + libadwaita, con paridad de features con la v0.2.x Python y empaquetado nativo.

**Architecture:** App GNOME nativa (gtk-rs plano + `Rc<RefCell>`/GObject, sin relm4 ni tokio en el main thread). El motor de sync sigue siendo `nextcloudcmd` (no se reimplementa). Rust es la capa de escritorio: config, credenciales, scheduling, watchers, tray, ventanas, logs, resolución de conflictos.

**Tech Stack:** Rust 1.97 (MSRV 1.83 ok), `gtk4 0.11` (feat `v4_22`), `libadwaita 0.9`, `glib 0.22` (feat `futures`), `ksni 0.3` (feat `blocking`), `notify 8`, `secret-service 5` (feat `rt-tokio-crypto-rust`), `async-channel 2`, `serde`/`serde_json`, `zbus 5` (push protocol). Todas verificadas contra docs.rs el 13-Ago-2026.

---

## Contexto / supuestos verificados

- **El motor se mantiene**: `nextcloudcmd` (binario) hace sync, conflictos y safety. Rust NO lo reimplementa. (REDESIGN §1, §3)
- **Tray**: ksni `blocking` + `glib::MainContext::invoke` para llegar a la UI. En GNOME stock hace falta la extensión AppIndicator (mismo requisito que hoy).
- **Iconos del tray**: la v0.2.5 publica glyphs Lucide monocolor (`cloud`/`cloud-off`) como pixmaps ARGB con trazo fijo (issue #22). Mantener ese contrato.
- **Patrón de concurrencia** (validado en spike): un solo `async-channel` + `glib::spawn_future_local` + `Rc<RefCell<AppState>>`; tray/notify/nextcloudcmd emiten, la main loop consume. `gio::spawn_blocking` para lo pesado.
- **⚠️ deadlock nextcloudcmd**: drenar stdout y stderr en paralelo (pipe 64 KB). (Hallazgo del spike, no verificado en prod real)
- **Config**: schema v6 (accounts con `folders` lista, posiblemente vacía, `remote_path` por folder; identity = hash server+login). Migrar a formato Rust o mantener compatibilidad de lectura.
- **Multi-cuenta + multi-folder**: cada folder = runtime propio (watcher, guard, exclusions, invocación), settings compartidas por cuenta. (REFACTOR-NOTES §2)
- **Versión actual a replicar**: v0.2.4+ (0.2.5 en ramas sin mergear). Empezar el rewrite desde `origin/main`.

## Inventario de features (de la v0.2.x, para paridad)

**Core/estado**: AccountManager (multi-cuenta), AccountRuntime (fachada RuntimeController — fix #20), FolderRuntime por folder, StateController/AggregateStateController, SyncPermit (semáforo 1-a-la-vez), scheduler (4 triggers), debounce, sync_engine (spawn nextcloudcmd + progreso).

**Triggers**: local_inotify, local_interval, remote_push, remote_interval. (REDESIGN §6)

**Safety (ligero, decisión 12-Ago)**: `delete_guard.py` (guard de borrado masivo, restore_from_server). El CLI NO protege contra borrado masivo local (verificado en código de nextcloud/desktop). Mantener un guard ligero.

**Integración GNOME**: tray (menú Open/Settings/Quit, issue #19), autostart, desktop integration (bookmark Nautilus, shortcut, icon), credenciales en Secret Service, updates check.

**UI**: main window (sidebar cuentas + NavigationSplitView), Settings (Adw PreferencesWindow), setup wizard (login flow v2 + first-sync dialog #8), conflict resolver (#7), recent activity + log view, About (Lucide info), update window, tray icon 2 estados.

**Privacidad/red**: redact de logs, network watcher, power/suspend.

## Fases y tareas

### Fase 0 — Fundaciones del crate

**Task 0.1: Esqueleto del proyecto**
- Crear: `nextsync-rs/` (o convertir el repo) con `cargo init --name nextsync`.
- `Cargo.toml` con las deps del spike (ver Tech Stack).
- Estructura: `src/main.rs`, `src/state.rs`, `src/core/`, `src/ui/`, `src/nextcloud/`, `src/storage/`, `src/util/`.
- Tests: `cargo test` compila y corre 1 smoke test.
- Commit: `chore: scaffold Rust crate`

**Task 0.2: CI básico**
- `.github/workflows/ci.yml`: `cargo fmt --check`, `cargo clippy -D warnings`, `cargo test`, `cargo build --release`.
- Commit: `ci: add Rust CI`

### Fase 1 — Config + credenciales (sin UI)

**Task 1.1: Modelo de config (schema v6)**
- `src/storage/config.rs`: structs serde `Config { general, accounts: Vec<AccountConfig> }`, `AccountConfig { id, server, login, folders: Vec<FolderConfig>, ... }`, `FolderConfig { local_root, remote_path, ... }`. Guardado en `~/.config/nextsync/` (o mismo path que Python: `~/.local/share/nextsync/` — verificar `util/paths.py`).
- Tests: serializar/deserializar, cuenta con folders vacíos, migración de schema v5→v6.
- Commit: `feat(config): Rust config model with schema v6`

**Task 1.2: Credenciales en Secret Service**
- `src/nextcloud/credentials.rs`: guardar/leer/borrar credencial por cuenta usando `secret-service` blocking (collection default, atributos por account_id). API validada en spike: `connect(EncryptionType::Dh)`, `create_item`, `Item::delete`.
- Tests: roundtrip con servicio real (requiere session; skip si no hay bus).
- Commit: `feat(credentials): Secret Service storage`

### Fase 2 — Estado y scheduling (núcleo, sin GTK)

**Task 2.1: StateController**
- `src/state.rs`: enum AppState (IDLE/SYNCING/PAUSED/DELETE_REVIEW...), `StateController` con suscripción (canal `async_channel`), `AggregateStateController` multi-folder.
- Tests: transiciones, agregación multi-folder.
- Commit: `feat(state): state machine with subscriptions`

**Task 2.2: SyncPermit + scheduler + triggers**
- `src/core/sync_permit.rs` (semáforo 1-a-la-vez FIFO), `src/core/scheduler.rs` (timers), `src/core/triggers.rs` (inotify/intervalo/push), `src/core/debounce.rs`.
- Tests: permit único, debounce, scheduling de los 4 triggers.
- Commit: `feat(core): sync permit, scheduler, triggers`

**Task 2.3: SyncEngine + progreso**
- `src/nextcloud/sync_engine.rs`: `std::process::Command::new("nextcloudcmd")` + args (`--trust`, `--httpproxy`, `--max-sync-retries`, `--exclude`, `--path`), stdout+stderr drenados en hilos paralelos (anti-deadlock), líneas → `async_channel` → `SyncProgress`.
- Tests: parser de progreso con fixtures (test_progress.py equivalente), línea por línea.
- Commit: `feat(sync): nextcloudcmd spawn with live progress`

### Fase 3 — Watchers y red

**Task 3.1: inotify + delete guard**
- `src/core/watcher.rs`: `notify` con `recommended_watcher()`, eventos → canal. `src/core/delete_guard.rs`: guard ligero de borrado masivo (umbral count/percent, manifest de última sync), `approve_delete_once` / `restore_from_server`.
- Tests: eventos fs, guard con umbral.
- Commit: `feat(watcher): filesystem monitoring and deletion guard`

**Task 3.2: network/power/suspend**
- `src/core/network.rs` (watcher online/offline), `src/core/power.rs` (pause on battery), `src/core/suspend.rs` (resume trigger).
- Tests: unit con fakes.
- Commit: `feat(core): network, power, suspend watchers`

### Fase 4 — Push (notify_push)

**Task 4.1: push protocol**
- `src/nextcloud/push.rs` + `push_protocol.rs`: WebSocket push sobre `zbus`? NO — WebSocket real. Verificar librería (hipótesis: `tungstenite` o `tokio-tungstenite` con runtime secundario, o `glib`-based). Estado en Python: `nextcloud/push.py` con libsoup3. **Riesgo alto**: elegir la librería correcta antes de codificar.
- Tests: contrato de mensajes (fixtures), reconexión.
- Commit: `feat(push): notify_push protocol`

### Fase 5 — UI (la parte grande)

**Task 5.1: Ventana principal**
- `src/ui/main_window.rs`: `Adw.ApplicationWindow` con `NavigationSplitView` (sidebar cuentas + content), label estado, progreso, botones header (Lucide settings-2/info — issue #21), Accent en botones (issue #16).
- Tests: smoke (construcción), contracts.
- Commit: `feat(ui): main window with account sidebar`

**Task 5.2: Settings**
- `src/ui/settings.rs`: `Adw.PreferencesWindow` con General/Sync/Network/Advanced + grupos por folder + Add Folder. Acepta un runtime (fachada). Empty state con cero folders (issue #17).
- Commit: `feat(ui): settings window`

**Task 5.3: Setup wizard**
- `src/ui/setup.rs`: login flow v2 (browser), first-sync confirmation dialog (issue #8, PROPFIND Depth 1).
- Commit: `feat(ui): account setup wizard`

**Task 5.4: Conflict resolver + activity + log**
- `src/ui/conflict_resolver.rs` (issue #7: keep local/remote/open), `src/ui/activity.rs` (recent), `src/ui/log_view.rs`.
- Commit: `feat(ui): conflict resolver, activity, log view`

**Task 5.5: Tray**
- `src/ui/tray.rs`: ksni blocking, menú Open/Settings/Quit (issue #19), icono Lucide `cloud`/`cloud-off` monocolor como pixmaps ARGB con trazo fijo (issue #22), callback→`MainContext::invoke`.
- Tests: contrato de iconos (monocolor, trazo fijo).
- Commit: `feat(tray): StatusNotifier tray with monochrome glyphs`

**Task 5.6: Autostart + desktop integration + updates**
- `src/core/autostart.rs`, `src/core/desktop_integration.rs`, `src/core/updates.rs` (check de releases GitHub, banner en UI).
- Commit: `feat(core): autostart, desktop integration, updates`

### Fase 6 — i18n, packaging, integración final

**Task 6.1: i18n**
- `gettext-rs` (o `fluent`) con locales EN + ES. Extraer strings con `_()`. Quitar el soporte PT-BR (decisión 12-Ago).
- Commit: `feat(i18n): ES/EN catalogs`

**Task 6.2: Packaging**
- PKGBUILD Arch (binario estático, `cargo build --release`), spec/DEB opcional, Flatpak manifest opcional.
- Commit: `build: packaging for Arch/Debian`

**Task 6.3: Paridad total + barrido de gaps**
- Checklist contra inventario de features. Comparar comportamiento con v0.2.x.
- `git diff` vs lista de features del plan; issues #16-#22 como rúbrica.
- Commit: `chore: parity checklist verification`

## Tests / validación

- Unit: `cargo test` (cada task tiene sus tests; el parser de progreso, config, state, guard, contrato de tray son los más valiosos).
- Smoke manual: la app arranca, abre Settings, tray registrado (`busctl --user get-property ... RegisteredStatusNotifierItems`), sync real contra una cuenta de prueba con `nextcloudcmd`.
- Métricas (contraste con Python): binario release <10 MiB, RSS idle <10 MiB (medir con `VmHWM` de /proc — `/usr/bin/time` no instalado).

## Riesgos / tradeoffs / preguntas abiertas

1. **Librería de WebSocket push** (Fase 4): riesgo alto. Verificar `tungstenite`/`tokio-tungstenite`/`glib`-native antes de codificar. Opción de degradación: `nextcloudcmd` en intervalos (los intervalos ya existen como trigger).
2. **Schema de config**: migrar el fichero Python v6 o arrancar schema nuevo. Decisión pendiente: compatibilidad de lectura del JSON existente (los usuarios con cuentas configuradas no deberían reconfigurar).
3. **Compatibilidad GNOME Shell**: el tray requiere extensión AppIndicator (igual que Python hoy). Documentado, no bloqueante.
4. **`notify_push`**: el push protocol con libsoup3 (Python) vs librería Rust — verificar compatibilidad con el servidor self-hosted (`Upgrade: h2,h2c` — el bug que motivó el REDESIGN). Una librería WebSocket estricta puede fallar igual que libsoup; hay que probar contra el servidor real.
5. **Scope**: decidir si el rewrite naturalmente descarta features de bajo valor (advanced logs) para cortar alcance — pregunta abierta del issue #15.
6. **Deadlock stdout/stderr** de `nextcloudcmd` no probado en prod real (solo `--help` en el spike).

## Orden de construcción recomendado

1-2-3 (fundaciones, config, estado, engine) → 4 (push, riesgo alto, cuanto antes) → 5.1-5.5 (UI) → 6 (i18n, packaging, paridad). Se puede iterar en paralelo por fases con subagentes una vez exista la Fase 1-2 estable.
