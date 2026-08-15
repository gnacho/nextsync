# Changelog

Todas las versiones notables de NextSync se documentan aquí. El formato sigue [Keep a Changelog](https://keepachangelog.com/es/1.1.0/) y el versionado es **+0.10 por release** (decisión del usuario, 14-Ago-2026).

## [0.72.0] - 2026-08-15

### Cambiado
- **Icono de sincronizado en verde (#61/#62)**: la carpeta sincronizada muestra un check verde (color de éxito sobre el emblema ok); mientras sincroniza, solo se ve el spinner girando (fuera el emblema estático).
- **Fila de conexión en el panel (#60)**: un globo verde centrado junto al host del servidor (sin esquema) al pie de los ajustes de la cuenta.

## [0.68.0] - 2026-08-15

### Cambiado
- **Resumen de cuenta en texto plano (#65)**: avatar, nombre de usuario, luz de estado y espacio usado, sin fondo de botón; la luz se actualiza en vivo.
- **"Añadir carpeta" es un botón (#66)** junto al resumen; los botones "Sincronizar ahora" y "Pausar sincronización" desaparecen de la vista de cuenta (la sincronización es automática y pausar vive en la bandeja).
- **Panel de cuenta mínimo (#67)**: fuera el grupo Servidor y los interruptores de integración de escritorio (eliminados de toda la app); Conexión, Autenticación y Quitar cuenta con sus descripciones.
- **"Deletion Guard" traducido**: "Protección contra borrados" (tenía la cadena vacía en el catálogo).

## [0.66.0] - 2026-08-15

### Corregido
- **Sincronización al arrancar y al añadir carpetas**: cada carpeta pide una sincronización inicial al montar los watchers (y al añadirla en caliente), de modo que los árboles se comparan desde el primer momento; el estado "Aún no se ha sincronizado" ya no se queda pegado hasta una ejecución manual.
- **El botón "Ajustes de la cuenta" muestra icono Y texto**: un botón plano de etiqueta-o-icono solo renderiza uno; ahora lleva ambos.

### Cambiado
- **Panel de cuenta compacto**: solo servidor, conexión (proxy/certificados), iniciar sesión de nuevo y quitar cuenta. Las opciones de sincronización (disparadores, exclusiones, fiabilidad, guard de borrado, salida detallada) vuelven a Preferencias como página "Sincronización"; el desplegable deja de ser gigante.

## [0.64.0] - 2026-08-15

### Corregido
- **El botón "Ajustes de la cuenta" ya abre el panel (#63)**: la fila era una ActionRow suelta (fuera de una lista), que en producción no emitía `activated` al hacer clic físico aunque funcionaba programáticamente. Ahora es un `gtk4::Button` con `connect_clicked`, que recibe el clic de forma fiable y alterna el panel.

## [0.62.0] - 2026-08-15

### Corregido
- **El panel "Ajustes de la cuenta" ya se despliega (#63)**: el panel usaba un ScrolledWindow anidado cuyo layout podía colapsar al alternarlo; ahora es un Revealer (desliza hacia abajo) que arranca oculto y se abre/cierra de forma fiable al pulsar la fila.
- **El panel está traducido al español**: todas las etiquetas (Ajustes de la cuenta, Conexión, Servidor, proxy, certificados) están en el catálogo es_ES; se limpiaron msgids duplicados.

## [0.58.0] - 2026-08-15

### Corregido
- **"Sincronizado" ya no miente (#59)**: una carpeta que nunca ha completado una sincronización mostraba "Sincronizado" al quedarse inactiva. Ahora hay un estado "Aún no se ha sincronizado" (con su propia luz y etiqueta de bandeja) que es el estado inicial y el que se muestra hasta que una sincronización real tenga éxito; un fallo ya no vuelve a "Sincronizado".

### Cambiado
- **Tarjeta de cuenta simplificada**: la cabecera de la ventana principal muestra "Conectado" y el espacio usado, sin repetir usuario@servidor (eso vive en la barra lateral y en los ajustes de la cuenta).
- **Dominios sin esquema**: la barra lateral muestra el host del servidor sin `https://`.

## [0.56.0] - 2026-08-15

### Cambiado
- **Labels de inicio de sesión más claros**: el botón de la página de carpetas ahora dice "Iniciar sesión" en vez de "Revisar configuración"; en OpenCloud el campo de contraseña se llama "Token de aplicación" y muestra una burbuja de información explicando que el token se crea en la web del servidor (App Tokens) y que la contraseña de la cuenta no vale ahí.

## [0.54.0] - 2026-08-15

### Cambiado
- **Subtítulo genérico**: la app soporta OpenCloud y Nextcloud, así que la ventana ya no menciona solo Nextcloud; ahora dice "Sincronización de archivos para GNOME".

## [0.52.0] - 2026-08-15

Correcciones del panel de ajustes y de la carpeta remota OpenCloud.

### Corregido
- **El panel "Ajustes de la cuenta" se renderiza bien**: los grupos de preferencias se mostraban en un contenedor plano sin el estilo de AdwPreferencesPage, por lo que parecía roto; ahora viven en una página real dentro de un scroll.
- **La carpeta remota OpenCloud vuelve a funcionar**: al añadir la primera carpeta de una cuenta OpenCloud, el flujo de revisión descartaba el space id descubierto y guardaba la carpeta sin él, así que la carpeta remota no se creaba ni sincronizaba. El space id ahora se conserva en la carpeta nueva.

## [0.100.0] - 2026-08-15

Credenciales que sí se guardan: la app usa el llavero de la sesión.

### Corregido
- **Credenciales en el llavero de la sesión (#58)**: la app guardaba las contraseñas en la colección por defecto del Secret Service, que en GNOME puede ser un llavero separado que nunca se desbloquea; cada escritura fallaba con un error de bloqueo y ninguna cuenta podía sincronizar (avatares incluidos). Ahora se prefiere el llavero `login` (el que la sesión desbloquea al iniciar), con la colección por defecto como respaldo, y el borrado solo toca elementos desbloqueados. Verificado: la suite de credenciales vuelve a pasar (antes 2 tests fallaban con el llavero bloqueado).

## [0.90.0] - 2026-08-15

Ajustes separados: lo de cada cuenta, en la ventana principal; lo global, en Preferencias.

### Cambiado
- **Panel de ajustes por cuenta (#56)**: la ventana principal gana una fila "Ajustes de la cuenta" bajo las carpetas sincronizadas que despliega las preferencias propias de la cuenta: servidor, proxy y confianza TLS, opciones de sincronización (cambios locales/remotos, exclusiones, reintentos), integración de escritorio, guard de borrado, salida detallada, autenticación ("Sign in again") y eliminación de la cuenta.
- **Preferencias solo globales (#56)**: la vista de Preferencias queda con General (arranque, notificaciones, horario de silencio), Red (allowlist Wi-Fi, impacto de transferencia) y Avanzado (logging, umbral de tamaño, copia de seguridad).

### Corregido
- **El proxy y la confianza TLS sí llegan al motor (#56)**: la producción creaba los runtimes con una configuración de red por defecto, así que el proxy configurado nunca se aplicaba. Proxy y trust pasan a ser campos por cuenta (con el valor global como respaldo) y la red efectiva se plumba a cada ejecución del motor, refrescada en caliente al cambiar los ajustes.

## [0.80.0] - 2026-08-15

OpenCloud: las carpetas remotas ya se crean solas.

### Corregido
- **Las carpetas remotas de OpenCloud se crean antes de sincronizar (#55)**: configurar una carpeta cuyo subdirectorio remoto no existía fallaba en silencio (`opencloudcmd` no crea carpetas y el ensurer excluía OpenCloud). Verificado contra un servidor real: MKCOL sobre el árbol WebDAV de spaces responde 201, así que ahora el ensurer crea el subdirectorio segmento a segmento bajo el space (raíz del space = no-op), igual que hace para Nextcloud.

## [0.72.0] - 2026-08-15

Ronda de limpieza de interfaz tras el uso real de la v0.60.0.

### Corregido
- **Las pestañas de Preferencias ya se ven (#51)**: la barra inferior de páginas (General / Sincronización / Red / Avanzado) nunca se mostraba porque el ViewSwitcherBar quedaba sin `reveal`; General era la única página alcanzable. Ahora siempre está visible.
- **La flecha "volver" solo tiene sentido en Preferencias (#54)**: desaparece de la cabecera cuando la vista de sincronización está en primer plano y reaparece sobre los ajustes.

### Cambiado
- **Menos botones en la cabecera (#52)**: fuera el botón de pausa global añadido con la v0.60.0; pausar/reanudar todo queda en el menú de la bandeja, donde se pidió.
- **La app sigue siempre el tema del sistema (#53)**: eliminado el selector claro/oscuro/sistema del menú (círculos CSS) y el override de arranque; el esquema guardado en configuración queda inerte por compatibilidad.

## [0.60.0] - 2026-08-15

La release grande: quince issues en tres frentes (seguridad de datos, red y control, interfaz), resueltos en paralelo.

### Seguridad de datos
- **Avisos bloqueantes antes de la primera sincronización (#35)**: cuando la carpeta local y la remota tienen datos a la vez, un diálogo bloqueante explica la fusión y los conflictos potenciales antes de arrancar. Las carpetas que ya estuvieron sincronizadas (detectadas por sus ficheros journal `.sync*`/`.db`) piden confirmación siempre, y los artefactos ocultos viejos se mandan a la papelera en vez de acumularse ("Start Fresh" frente a "Keep Synchronization History").
- **Nunca dos syncs solapadas (#35)**: el permiso de sincronización rechaza carreras sobre la misma carpeta o padre/hijo (rutas canónicas) y un escaneo de `/proc` detecta un motor externo (nextcloudcmd/opencloudcmd) corriendo sobre la misma carpeta antes de arrancar.
- **Confirmación por tamaño (#36)**: ajuste nuevo (Synchronization) con umbral en MiB (500 por defecto, 0 lo desactiva); la primera sincronización estima el tamaño remoto (PROPFIND con getcontentlength; OpenCloud por quota del space) y pide confirmación por encima del umbral, recordándolo por carpeta.
- **Restaurar desde la papelera del servidor (#38)**: el diálogo de revisión de borrados gana "Restore from server trash", que lista la papelera WebDAV de Nextcloud (nombre, ubicación original, fecha) y restaura con MOVE al endpoint restore (oculto en OpenCloud, sin papelera documentada).
- **Cambios pendientes antes de sincronizar (#46)**: nueva entrada por carpeta que muestra el diff local contra el journal (nuevos/modificados/borrados, acotado a 50 con contador), calculado fuera del hilo de UI.

### Red y control
- **Menor impacto de transferencia (#39)**: ajuste nuevo que lanza el motor con `ionice -c 3` + `nice -n 10` cuando existen en el sistema; documentado como prioridad, no límite de velocidad.
- **Redes con límite de datos (#40)**: el planificador bloquea las carreras automáticas en conexiones medidas (NetworkMonitor de GLib); las manuales pasan siempre, y al salir de la red medida el trabajo pendiente se relanza.
- **Solo en redes Wi-Fi elegidas (#41)**: lista de SSIDs permitidos (coma separada); una red fuera de la lista pausa la sincronización como si estuviera offline, y se reevalúa en cada cambio de red.
- **Horario de silencio (#45)**: ventana diaria HH:MM (soporta cruce de medianoche) durante la cual no arrancan carreras nuevas; las en curso terminan.
- **Pausar todo (#42)**: botón en la cabecera y entrada "Pausar todo"/"Reanudar todo" en el menú del tray que conmutan la pausa de todas las cuentas a la vez, con icono y etiqueta siguiendo el estado.
- **Copia de seguridad de la configuración (#47)**: exportar (cuentas, carpetas, preferencias a JSON) e importar con validación por el mismo loader y reemplazo atómico, en la página Avanzado; los secretos del llavero nunca viajan en el fichero.

### Interfaz
- **Cambiar de cuenta cambia la vista (#49)**: la lista lateral ahora responde a la selección (antes la vista solo se presentaba al arranque) y mantiene el resaltado de la cuenta activa al refrescar.
- **Al quitar carpeta, a la papelera (#37)**: diálogo con "Move Folder to Trash" (papelera del sistema vía GIO) o "Keep Folder" para solo desconfigurar.
- **Tamaño local por carpeta (#43)**: cada fila muestra su tamaño en disco ("12.4 GiB local"), medido fuera del hilo de UI y refrescado al terminar cada sincronización.
- **Emblema de estado en el gestor de ficheros (#44)**: las carpetas sincronizadas llevan emblema según su estado (sincronizando, error, pausada, correcto) vía metadatos gvfs; verificado contra el daemon real (nota: dos de los emblemas no existen aún en Adwaita, documentado).
- **Avatar de la cuenta (#50)**: la tarjeta de cuenta y la barra lateral muestran la foto del usuario (Nextcloud `/avatar`, OpenCloud Graph), con caché en disco y placeholder de iniciales.

## [0.50.0] - 2026-08-15

OpenCloud verificado contra un servidor real: LibreGraph sustituye al raíz WebDAV.

### Corregido
- **Validación y descubrimiento OpenCloud sobre LibreGraph (#48)**: verificado contra un despliegue real de OpenCloud, el raíz de spaces WebDAV responde 405 a PROPFIND, así que la validación de v0.40.0 no funcionaba en la práctica. La validación ahora lee `GET /graph/v1.0/me` (que además devuelve el nombre para mostrar) y el listado de spaces lee `GET /graph/v1.0/drives`: se conservan el space personal propio y los de proyecto, y se excluyen los personales de otros usuarios y el agregado virtual de compartidos. El probe del primer sync mantiene el PROPFIND sobre la URL concreta del space (responde 207).

## [0.40.0] - 2026-08-15

OpenCloud sync deja de ser una suposición: la autenticación se verifica contra el endpoint documentado del servidor.

### Añadido
- **Validación de credenciales OpenCloud (#48)**: username + app token se validan con un PROPFIND sobre `/remote.php/dav/spaces/` (el punto de entrada WebDAV que la documentación de OpenCloud define para clientes externos, `Basic user:app-token`). Un token malo o expirado se rechaza con el mismo error de credenciales que Nextcloud; el fallback anterior usaba el endpoint OCS de Nextcloud, que los servidores OpenCloud no exponen.
- **Listado nativo de spaces (#48)**: el asistente descubre los spaces con un PROPFIND Depth-1 sobre el mismo árbol WebDAV (id = último segmento del href, nombre = `<d:displayname>`), sin depender de `opencloudcmd` para el descubrimiento. El modo query del CLI queda como fallback.
- **Probe del space para el primer sync (#48)**: el diálogo de confirmación del primer sync sondea la raíz del space (no la ruta `files/` de Nextcloud, inexistente en OpenCloud), así que avisa correctamente de espacios remotos no vacíos.

## [0.30.0] - 2026-08-14

Corrección del selector de tema para que coincida con el patrón de GNOME Text Editor.

### Cambiado
- **Selector de tema rediseñado (#27)**: ahora coincide con el patrón de **GNOME Text Editor**. Círculos de 44px sólidos, sistema partido diagonalmente (blanco arriba-izquierda / negro abajo-derecha), claro y oscuro como círculos sólidos, anillo de acento en el seleccionado y check como badge en la esquina inferior derecha.

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
