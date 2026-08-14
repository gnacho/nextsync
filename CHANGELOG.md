# Changelog

Todas las versiones notables de NextSync se documentan aquí. El formato sigue [Keep a Changelog](https://keepachangelog.com/es/1.1.0/) y el versionado es **+0.10 por release** (decisión del usuario, 14-Ago-2026).

## [0.20.0] - 2026-08-14

Iteración de estabilidad y usabilidad: cierre a bandeja, ventana de actividad legible, selector de tema más grande, renombrado de menús, setup sin ventana estirada, revocación de app passwords y notificaciones del servidor. Repo renombrado a `gnacho/nextsync`.

### Añadido
- **Notificaciones del servidor (#31)**: nueva opción en Ajustes → General que muestra las notificaciones propias del servidor (compartidos, comentarios, menciones) como notificaciones de escritorio. Consulta vía API OCS de notificaciones con deduplicación y primer fetch silencioso para no spamear el historial.
- **Revocación de app password al quitar cuenta (#28)**: al eliminar una cuenta se revoca el app password en el servidor (`DELETE /ocs/v2.php/core/apppassword`) y se limpia la entrada del llavero (keyring), best-effort para no bloquear la eliminación local si el servidor no responde.

### Cambiado
- **Cerrar la ventana minimiza a bandeja (#34)**: cerrar la ventana principal ya no cierra la app; queda en el systray y el cierre real se hace desde el item Quit del tray. Si no hay tray disponible, mantener el comportamiento anterior.
- **Selector de tema más grande (#27)**: los círculos claro/oscuro/auto del selector de tema pasan de 26px a 34px, manteniendo el área táctil.
- **Item del systray "Log" (#32)**: el menú del tray renombra "Sync Activity and Conflicts…" a "Log" (Registro), con msgid separado para no tocar el título de la ventana.
- **Ventana de actividad/conflictos legible (#33)**: las líneas del registro muestran `login@servidor · ruta local` en vez de IDs opacos, mejor reparto del espacio horizontal y filas de conflictos adaptables a ventanas estrechas.
- **URL de login del flujo de navegador truncada (#29)**: el label de la URL de login se muestra truncado con "…" para que la ventana del asistente no se estire; la URL completa sigue copiable y seleccionable.

### Repositorio
- Renombrado `gnacho/nextsync-rs` → `gnacho/nextsync`; todas las referencias (código, README, PKGBUILD, plan) actualizadas. El nombre antiguo redirige automáticamente.

## [0.10.0] - 2026-08-14

- Selector de tema claro/oscuro/auto dentro del menú con círculos CSS, e integraciones de escritorio restauradas (#25).

## [0.9.0] - 2026-08-14

- Tarjeta de resumen de cuenta, notificaciones de escritorio, selector de tema y flecha de volver estándar (#20-#23).

## [0.8.0] - 2026-08-14

- Ajustes simplificados y tema sistema/claro/oscuro (#17-#18).

## [0.7.0] - 2026-08-14

- Hot refresh de carpetas, creación de carpeta remota (MKCOL) y eliminación de la ventana de log (#13-#15).

## [0.6.0] - 2026-08-14

- Ajustes in-app, flujo de login con navegador v2 e interop de llavero con la app Python (#9-#12).

## [0.5.0] - 2026-08-14

- "Sign in again", fix de SIGILL y cableado del core (#1).

## [0.2.0] - 2026-08-13

- Primera release del rewrite en Rust (GTK4/libadwaita): cuentas, carpetas, sincronización vía nextcloudcmd, bandeja, ajustes y asistente de configuración.
