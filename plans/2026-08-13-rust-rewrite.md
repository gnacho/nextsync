# NextSync Rust Rewrite — Implementation Plan

> **Decisiones (13-Ago-2026, usuario A FUEGO):** refactor a Rust aprobado. Se abandona la caza de bugs de la línea Python (el bug de Settings de ryzen-ai queda sin diagnosticar). Todo el conocimiento de git se usa para el rewrite: issues #4/#5/#7/#8/#9/#12/#13/#16-#22, `docs/REDESIGN.md`, `docs/REFACTOR-NOTES.md`, CHANGELOG, arquitectura v0.2.5 y spike validado en `/tmp/opencode/spike-ncsync`.

**Actualización paridad (13-Ago-2026):** el Python avanzó a **v0.3.0** con features nuevas que el plan original no recogía. Se añaden a paridad (ver §Delta v0.2.5→v0.3.0): #25 (remote folder picker + auto-name), #33 (per-folder sync status), #30/#31 (log row + conflictos solo presentes), #32 (accent en Settings post-#16), fixes v0.2.6-v0.2.9. El plan se actualiza contra **origin/main v0.3.0**, no v0.2.4.

**Goal:** Reescribir NextSync (wrapper fino sobre `nextcloudcmd`) en Rust + gtk-rs + libadwaita, con paridad de features con la v0.3.0 Python y empaquetado nativo.

**Architecture:** App GNOME nativa (gtk-rs plano + `Rc<RefCell>`/GObject, sin relm4 ni tokio en el main thread). El motor de sync sigue siendo `nextcloudcmd` (no se reimplementa). Rust es la capa de escritorio: config, credenciales, scheduling, watchers, tray, ventanas, logs, resolución de conflictos.

**Tech Stack:** Rust 1.97 (MSRV 1.83 ok), `gtk4 0.11` (feat `v4_22`), `libadwaita 0.9`, `glib 0.22` (feat `futures`), `ksni 0.3` (feat `blocking`), `notify 8`, `secret-service 5` (feat `rt-tokio-crypto-rust`), `async-channel 2`, `serde`/`serde_json`. **Push protocol:** `tungstenite 0.30` (feature `rustls-tls-native-roots`), en hilo `gio::spawn_blocking`, **handshake manual tolerante** + `WebSocket::from_partially_read` (mitiga el bug `Upgrade: h2,h2c` de openresty; los PRs #548/#549 de tungstenite que lo arreglarían siguen sin mergear). Verificado 13-Ago-2026: la familia tungstenite/tokio-tungstenite/async-tungstenite comparte validación estricta de handshake; GIO no tiene WebSocket nativo; libsoup3 es el origen del bug en Python. `zbus 5` solo si hace falta (login/OCS).

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

## Proveedor dual Nextcloud / OpenCloud (decisión 13-Ago-2026, A FUEGO)

La siguiente release soporta **ambos proveedores** seleccionables por cuenta. Investigación verificada (auth de `opencloudcmd` en código real, docs oficiales):

**Abstracción (Task nueva 0.4, antes que cualquier UI)**: trait `SyncDriver` que encapsula el motor CLI:
- `NextcloudDriver`: binario `nextcloudcmd`, args `[--trust] [--httpproxy] [--max-sync-retries N] [--exclude fichero] [--path /remoto] <local_root> <server_url>`, credenciales por env `NC_USER`/`NC_PASSWORD`.
- `OpenCloudDriver`: binario `opencloudcmd`, args `<server_url> <space_id> <source_dir> --user U --token T [--remote-folder] [--exclude] [--max-sync-retries]`, credenciales por flag (app password persistente).
- El **parser de progreso es compartido** (ambos CLI son forks de la misma base Qt, mismo formato de salida) — `nextcloudcmd_progress.rs` vale para los dos.
- `sync_engine.rs` ya recibe el binario/config vía `CommandSpec` → parametrizar por `Provider`, no duplicar el engine.

**Config (Task 1.1 actualizada)**: campo `provider: "nextcloud" | "opencloud"` por cuenta (default `nextcloud`). `FolderConfig` gana `space_id` (OpenCloud) junto a `remote_path` (Nextcloud); ambos opcionales según proveedor.

**Auth por proveedor**:
- Nextcloud → login flow v2 (OAuth browser) + credenciales en keyring (como hoy).
- OpenCloud → **app password persistente** (creada en web del servidor o `opencloud auth-app create --expiration 72h`), guardada en keyring, reutilizable indefinidamente; 401 → pedir otra. Sin OAuth ni device flow (el CLI no lo soporta; lico no ofrece device flow).

**Push**: solo Nextcloud (notify_push). OpenCloud no tiene notify_push → ese trigger queda deshabilitado y se usa `remote_interval` polling.

**Setup wizard (Task 5.3 actualizada)**: selector de proveedor al crear cuenta → flujo Nextcloud (browser OAuth) o flujo OpenCloud (server_url + user + app password; `space_id` autodescubierto con `opencloudcmd <url>`).

**Empaquetado (Task 6.2)**: depender de `nextcloud-client` (nextcloudcmd) y opcionalmente `opencloud-desktop-git`/build del desktop oficial (opencloudcmd). La app detecta binarios presentes; si el proveedor elegido no tiene binario, aviso en setup.

**Ejemplo de uso (confirmado por diseño)**: una carpeta compartida con Nextcloud y otra con OpenCloud = **dos cuentas** (una `provider: nextcloud` con sus `folders[]`, otra `provider: opencloud` con sus `folders[]` y `space_id`). El `provider` vive en la cuenta, no en la carpeta: no se mezclan proveedores dentro de un mismo login (difieren en binario CLI, args y credenciales). Cada cuenta tiene su propio estado, triggers, delete guard y credenciales; el scheduler, SyncPermit y la UI multi-cuenta son compartidos.

## Inventario de features (de la v0.2.x, para paridad)

**Core/estado**: AccountManager (multi-cuenta), AccountRuntime (fachada RuntimeController — fix #20), FolderRuntime por folder, StateController/AggregateStateController, SyncPermit (semáforo 1-a-la-vez), scheduler (4 triggers), debounce, sync_engine (spawn nextcloudcmd + progreso).

**Triggers**: local_inotify, local_interval, remote_push, remote_interval. (REDESIGN §6)

**Safety (ligero, decisión 12-Ago)**: `delete_guard.py` (guard de borrado masivo, restore_from_server). El CLI NO protege contra borrado masivo local (verificado en código de nextcloud/desktop). Mantener un guard ligero.

**Integración GNOME**: tray (menú Open/Settings/Quit, issue #19), autostart, desktop integration (bookmark Nautilus, shortcut, icon), credenciales en Secret Service, updates check.

**UI**: main window (sidebar cuentas + NavigationSplitView), Settings (Adw PreferencesWindow), setup wizard (login flow v2 + first-sync dialog #8), conflict resolver (#7), recent activity + log view, About (Lucide info), update window, tray icon 2 estados.

**Privacidad/red**: redact de logs, network watcher, power/suspend.

## Delta v0.2.5 → v0.3.0 (features/fixes nuevos en Python, pendientes de paridad)

Verificado contra origin/main v0.3.0 el 13-Ago-2026:

| Commit/merge | Feature/fix | Impacto en Rust |
|---|---|---|
| #25 `eaf703a` | **Remote folder picker + auto-name**: `NextcloudApi.list_remote_folders(server, user)` hace PROPFIND contra `/remote.php/dav/files/{user}` y lista las carpetas top-level existentes como `"/nombre"`; `remote_path_for(local_root, text)` auto-nombra un remote_path en blanco con el nombre de la carpeta local (`/home/user/NextCloud` → `/NextCloud`). | Nuevo endpoint API WebDAV en Rust + helper de auto-nombre. Fase de UI Settings (5.2). |
| #33 `4f87994` | **Per-folder sync status**: `ui/folder_status.py` (126 líneas) con `folder_status_presentation(state)` (etiqueta+icono por estado) y `FolderStatusRow` (Adw.ActionRow que se suscribe al estado de cada folder y renderiza). Main window muestra una fila por folder con su estado. | Fase 5.1 main window: row por folder con estado individual (ya tenemos StateController por folder; falta la presentación). |
| #30/#31 `00badaf` | **Log row siempre attachada + conflictos solo si presentes**: fix de la Activity view (log row no se despega; la fila de conflictos solo aparece cuando hay conflicted copies). | Detalle de Fase 5.4 (activity/conflict view). |
| #32 `3865d13` | **Settings controls al accent** (extensión de #16): test de contrato que fija que los controles de Settings post-#16 siguen el accent. | Ya cubierto por diseño Rust (adwaita nativo usa accent); añadir test de contrato equivalente. |
| `f647328` | **Refresh main window cuando cierra Settings**: al cerrar Settings se refresca la ventana principal. | Conectar señal close-request de Settings → refresh de main window (Fase 5.1/5.2). |
| `36b59c5` | **Pick up added/removed folders sin restart**: `AccountRuntime` reconfigura en caliente cuando cambian los folders de una cuenta. | AccountManager Rust debe reconfigurar FolderRuntimes en caliente (añadir/eliminar). Fase de core. |
| `f71ea90` | Fix Settings: `set_placeholder_text` → título compatible con EntryRow. | Detalle UI (Fase 5.2). |
| `a261c31` | Fix Settings: `delete_guard` leído con default para evitar KeyError. | ConfigStore Rust ya usa `#[serde(default)]` → cubierto. |

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

**Task 0.4: Abstracción de proveedor (SyncDriver)**
- `src/nextcloud/driver.rs`: enum `Provider { Nextcloud, OpenCloud }` + trait `SyncDriver` con `build_command(account, folder, network, credentials) -> CommandSpec` y `binary_name()`.
- `NextcloudDriver` y `OpenCloudDriver` (args y credenciales según §Proveedor dual).
- Refactor: `command.rs`/`sync_engine.rs` parametrizados por `Provider` (el parser de progreso es compartido).
- Tests: `build_command` por proveedor (args exactos), binarios detectados.
- Commit: `feat(driver): provider abstraction for Nextcloud and OpenCloud`

### Fase 1 — Config + credenciales (sin UI)

**Task 1.1: Modelo de config (schema v7, doble proveedor)**
- `src/storage/config.rs`: structs serde `Config { general, accounts: Vec<AccountConfig> }`, `AccountConfig { id, server, login, provider, folders: Vec<FolderConfig>, ... }`, `FolderConfig { local_root, remote_path, space_id, ... }`. `provider: "nextcloud" | "opencloud"` (default `nextcloud`). Guardado en `~/.config/nextsync/` (o mismo path que Python: `~/.local/share/nextsync/` — verificar `util/paths.py`).
- Leer el schema v6 Python como entrada y migrar a v7 (añade `provider`, opcional en lectura).
- Tests: serializar/deserializar, cuenta con folders vacíos, migración v5→v6→v7, provider por defecto.
- Commit: `feat(config): Rust config model with schema v7 and provider`

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
- **Per-folder sync status (#33)**: una `FolderStatusRow` por folder con su estado individual (replica `folder_status.py`: etiqueta+icono por estado via `folder_status_presentation`).
- **Refresh al cerrar Settings (v0.2.9)**: conectar close-request de Settings → refrescar main window.
- Tests: smoke (construcción), contracts, presentación por estado de folder.
- Commit: `feat(ui): main window with account sidebar and per-folder status`

**Task 5.2: Settings**
- `src/ui/settings.rs`: `Adw.PreferencesWindow` con General/Sync/Network/Advanced + grupos por folder + Add Folder. Acepta un runtime (fachada). Empty state con cero folders (issue #17). Fix EntryRow title (v0.2.7).
- **Remote folder picker (#25)**: al configurar una carpeta, listar las carpetas remotas existentes (`list_remote_folders` PROPFIND contra `/remote.php/dav/files/{user}`) y ofrecerlas; `remote_path_for(local_root, text)` auto-nombra un blank con el nombre de la carpeta local.
- Tests: contrato accent (#32), empty state, picker con fixtures PROPFIND.
- Commit: `feat(ui): settings window with remote folder picker`

**Task 5.3: Setup wizard**
- `src/ui/setup.rs`: **selector de proveedor** (Nextcloud / OpenCloud) al crear cuenta.
  - Nextcloud → login flow v2 (browser OAuth), first-sync confirmation dialog (issue #8, PROPFIND Depth 1).
  - OpenCloud → server_url + user + **app password** (campo de contraseña), `space_id` autodescubierto con `opencloudcmd <url>` (lista de spaces). Aviso si el binario `opencloudcmd` no está presente.
- Commit: `feat(ui): account setup wizard with provider selection`

**Task 5.4: Conflict resolver + activity + log**
- `src/ui/conflict_resolver.rs` (issue #7: keep local/remote/open), `src/ui/activity.rs` (recent), `src/ui/log_view.rs`.
- **Fix v0.2.9 (#30/#31)**: log row siempre adjunta a la actividad; la fila de conflictos solo aparece si hay conflicted copies.
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
