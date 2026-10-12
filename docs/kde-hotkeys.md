# Глобальные хоткеи KDE Plasma: как работают пользовательские командные хоткеи (исследование M4)

Исследовано 2026-10-12 на живом хосте: CachyOS, **Plasma 6.7.5**
(`plasmashell 6.7.5`, `kwin 6.7.5`), сессия Wayland. Все утверждения ниже
проверены по исходникам KDE (GitHub-зеркала invent.kde.org) и локальным
D-Bus-интерфейсам, не угаданы.

## Где живут хоткеи

- **`~/.config/kglobalshortcutsrc`** — реестр хоткеев (KConfig INI). Один
  раздел на «компонент». Компоненты — это либо группы приложений/действий
  (например `[kmix]`), либо **сервисные компоненты**: разделы с именами
  `[services][<storage-id desktop-файла>]`.
- В современном формате каждое действие сервиса — один ключ, хранящий
  только *текущий* хоткей, например `_launch=Alt+1` (несколько клавиш
  соединяются `\t`). Старый формат-тройка `current,default,имя` доживает
  только в легаси-группах верхнего уровня и мигрируется демоном
  (`GlobalShortcutsRegistry::migrateConfig`).
- Семантика переопределения: запись пишется **только когда текущий хоткей
  отличается от дефолтного**. Дефолт берётся из записи `X-KDE-Shortcuts`
  в `.desktop`-файле (`KServiceActionComponent::loadSettings`:
  `configGroup.readEntry("_launch", defaultFromDesktopFile)`).

## Как исполняется командный хоткей

Для сервисных компонентов демон не сигналит запущенному приложению — он
сам запускает desktop-файл: `KServiceActionComponent::emitGlobalShortcutEvent`
создаёт `KIO::ApplicationLauncherJob(m_service)` для действия `_launch`.
Значит, по нажатию клавиши исполняется строка `Exec` desktop-файла.

## Где демон находит командные desktop-файлы

Два пути обнаружения (`GlobalShortcutsRegistry::loadSettings` /
`refreshServices` / `detectAppsWithShortcuts`):

1. **`<XDG data dirs>/kglobalaccel/*.desktop`** — разбираются в сервисные
   компоненты **только при старте демона**. Файлы с `NoDisplay=true`
   пропускаются. `refreshServices` по этому пути только подчищает
   мёртвые записи, новых в рантайме не добавляет.
2. **Приложения со свойством `X-KDE-Shortcuts`** — находятся через
   `KApplicationTrader::query` (база ksycoca, включающая
   `~/.local/share/applications/`), обновляются каждый раз, когда ksycoca
   сигналит `databaseChanged` (демон подключает `KSycoca::databaseChanged`
   → `refreshServices`). `KApplicationTrader::query` фильтрует только
   `OnlyShowIn`/`NotShowIn` (`showInCurrentDesktop`); **`NoDisplay=true`
   сервис не исключает**. Путь (2) поэтому поддерживает живой подхват
   после `kbuildsycoca6`, без перелогина.

## Что делает сам Параметры системы (Добавить → Команда или сценарий)

`KCMKeys::addCommand` в `plasma-desktop/kcms/keys/kcm_keys.cpp`:

- пишет
  `~/.local/share/applications/net.local.<executable>...desktop` с
  `Type=Application`, `Name`, `Exec` (`%` экранируется в `%%`),
  `NoDisplay=true`, `StartupNotify=false`,
  `X-KDE-GlobalAccel-CommandShortcut=true`;
- подталкивает демона по D-Bus (`doRegister` + `unregister`), чтобы новый
  компонент появился немедленно;
- сама привязка клавиши назначается потом в UI и попадает в
  `~/.config/kglobalshortcutsrc` как `[services][<id>.desktop] _launch=...`.

## Почему мы НЕ пишем kglobalshortcutsrc напрямую

Файл принадлежит и живьём переписывается сервером хоткеев (в Plasma 6.4+
на Wayland весь код kglobalacceld работает **внутри `kwin_wayland`** — на
этом хосте `org.kde.kglobalaccel` принадлежит PID `kwin_wayland`;
автономный бинарник `/usr/lib/kglobalacceld` и
`plasma-kglobalaccel.service` существуют, но не запущены). Правка
живьём-управляемого KConfig-файла из второго процесса рискует потерянными
обновлениями и порчей файла, а ошибочная запись может сломать хоткеи
пользователя. D-Bus API (`org.kde.KGlobalAccel`, `/kglobalaccel`, см.
`/usr/share/dbus-1/interfaces/kf6_org.kde.KGlobalAccel.xml`) регистрирует
действия только для *запущенных приложений, получающих сигнал нажатия* —
регистрации «запусти вот эту команду» там нет; командные хоткеи по
замыслу — файловый механизм.

## Дизайн `--write` в soundboard (безопасный автоматический путь)

Для каждого из первых 9 клипов (по алфавиту) пишется ОДИН самодостаточный
файл:

```
~/.local/share/applications/net.local.soundboard.<slug>.desktop
```

```ini
# Managed by "soundboard hotkeys --write". Editing is safe; delete to remove.
[Desktop Entry]
Type=Application
Name=Soundboard: <клип>
Exec="</абсолютный/путь/soundboard>" play "<экранированное имя клипа>"
NoDisplay=true
StartupNotify=false
X-KDE-GlobalAccel-CommandShortcut=true
X-KDE-Shortcuts=Alt+N
```

- `X-KDE-Shortcuts=Alt+N` — *дефолтный* хоткей, читаемый прямо из
  desktop-файла; `kglobalshortcutsrc` остаётся нетронутым (записи
  переопределения нужны только для нестандартных клавиш).
- Обнаружение идёт через ksycoca (путь 2 выше): в `Exec` подставляется
  абсолютный путь запущенного бинарника soundboard (через
  `current_exe()`), поэтому хоткеи не зависят от `PATH` сессии. После
  перемещения или переустановки бинарника в другое место перерегистрируйте
  хоткеи.
- Экранирование `Exec` по Desktop Entry Specification: всё имя клипа —
  один аргумент в двойных кавычках; внутри кавычек `\`, `"`, `` ` ``, `$`
  экранируются обратным слешем, буквальные новая строка/табуляция/CR —
  последовательностями `\n`/`\t`/`\r`; любой `%` удваивается до `%%`.
- Идемпотентность: детерминированные имена файлов (слаг клипа, `-2`/`-3`
  при коллизии) и содержимое, выводимое чисто из `(клип, клавиша)`;
  повторные запуски распознают неизменённые файлы. Перезапись — только
  когда файл несёт наш маркер-комментарий генератора; чужой файл с тем же
  именем не трогается никогда. Перед перезаписью прежнее содержимое
  бэкапится рядом с меткой времени.

## Управление из приложения (GUI)

GUI на вкладке «Настройки» управляет теми же файлами в своём процессе
(CLI не нужен): рисует таблицу клип → клавиша по текущим клипам,
определяет состояние регистрации сканом `~/.local/share/applications/` на
предмет наших файлов `net.local.soundboard.*.desktop` с маркером и
сравнением набора имён+содержимого с желаемым («Хоткеи
зарегистрированы» / «Хоткеи не зарегистрированы» / «Хоткеи устарели —
…»), пишет файлы ровно тем же путём `write_desktop_files`, что и
`--write` (идемпотентно, с бэкапами, чужие файлы не трогаются), и затем
автоматически выполняет живое обновление из следующего раздела.
«Выключить» удаляет только наши файлы с маркером (бэкапы и чужие
двойники переживают) и обновляет ksycoca. Правила активации из следующего
раздела применимы без изменений.

## Живое обновление после перезаписи (ловушка «уже есть»)

Проверено живьём 2026-10-12 на Plasma 6.7.5: демон читает `Exec`
сервисного компонента ровно один раз — при старте (`loadSettings`) и когда
впервые замечает компонент. По ksycoca `databaseChanged`
`refreshServices`/`detectAppsWithShortcuts` явно пропускают компоненты,
которые уже существуют (`// already there` → `continue`). Перезапись
существующего desktop-файла (например, новый `Exec` после переезда
бинарника) оставляет демона со СТАРЫМ сервисом: хоткей срабатывает, но
запуск падает — `kwin_wayland` (где живёт kglobalacceld на 6.4+ Wayland)
пишет в журнал «Невозможно найти программу "soundboard"» — при уже
правильном файле на диске.

Демон пересоздаёт компонент, только когда его storage-id снова появляется
после исчезновения (`refreshServices` вычищает компоненты, чей
`KService::serviceByStorageId` больше не разрешается). Единственное
надёжное живое обновление — «танец» скрыть/вернуть, реализованный в
`src/ui/kglobalaccel.rs` (`refresh_services`) и выполняемый ОБАМИ путями:
CLI `hotkeys --write` и GUI «Включить/Обновить хоткеи»:

1. переименовать наши `net.local.soundboard.*.desktop` в сторону и
   выполнить `kbuildsycoca6`;
2. опрашивать `org.kde.KGlobalAccel.allComponents`, пока компоненты НЕ
   ИСЧЕЗНУТ — обязательно: проход обновления, который ещё видит файлы,
   оставит устаревший объект сервиса;
3. вернуть файлы на место, снова `kbuildsycoca6`;
4. опрашивать, пока все компоненты не зарегистрируются снова (уже со
   свежим `Exec`).

Без демона (не-Plasma сессия) «танец» вырождается в обычный
`kbuildsycoca6`. Пользовательские переопределения в
`kglobalshortcutsrc` переживают «танец»:
`KServiceActionComponent::loadSettings` перечитывает их при
пересоздании.

## Проверка регистрации и срабатывание без клавиатуры

- Зарегистрированные компоненты:
  `gdbus call --session --dest org.kde.kglobalaccel --object-path
  /kglobalaccel --method org.kde.KGlobalAccel.allComponents` (в путях
  объектов компонентов точки заменены на подчёркивания:
  `/component/net_local_soundboard_bruh_desktop`).
- Проверка привязки:
  `gdbus call … --object-path /component/<id> --method
  org.kde.kglobalaccel.Component.allShortcutInfos` — код клавиши в
  кодировке Qt, `Alt+1` = `0x8000000 | 0x31` = `134217777`.
- Программный запуск (без клавиатуры; клип играет по-настоящему):
  `gdbus call … --object-path /component/<id> --method
  org.kde.kglobalaccel.Component.invokeShortcut '_launch'` — демон
  запускает desktop-файл через `KIO::ApplicationLauncherJob`; на этом
  хосте запуск виден в `journalctl --user` как
  `Started Soundboard: Bruh.` и как процесс
  `…/soundboard play <клип>`.
- Неудачные запуски пишет `kwin_wayland`:
  `journalctl --user | grep "Невозможно найти программу"` — верный признак
  ловушки stale Exec выше (или несуществующего пути бинарника).

## Активация

- Обычный случай: ручных шагов нет — `soundboard hotkeys --write` и GUI
  «Включить/Обновить хоткеи» сами выполняют полное живое обновление
  («танец» скрыть/вернуть + два `kbuildsycoca6`, см. выше).
- Гарантированный запасной вариант: выйти и зайти в сессию снова (реестр
  загружает все пути при старте). На сессиях с автономным демоном (X11)
  работает и `systemctl --user restart plasma-kglobalaccel.service`; на
  Plasma 6.4+ Wayland, как на этом хосте, сервер живёт в kwin — перезапуск
  этой юниты ради перечитывания не поможет.

## Ручной путь (чему учит вывод `soundboard hotkeys`)

Параметры системы → Клавиатура → Комбинации клавиш → **Добавить →
Команда или сценарий…**, вставить `soundboard play <клип>`, назначить
`Alt+N`.

## Источники

- Демон kglobalacceld: `GlobalShortcutsRegistry` (load/refresh/migrate),
  `KServiceActionComponent` (launch, write/load) —
  <https://github.com/KDE/kglobalacceld> (`src/globalshortcutsregistry.cpp`,
  `src/kserviceactioncomponent.cpp`).
- KCM хоткеев `addCommand` —
  <https://github.com/KDE/plasma-desktop> (`kcms/keys/kcm_keys.cpp`,
  `kcms/keys/globalaccelmodel.cpp`).
- Семантика запроса приложений ksycoca —
  <https://github.com/KDE/kservice> (`src/services/kapplicationtrader.cpp`).
- Поверхность D-Bus — локальная интроспекция:
  `/usr/share/dbus-1/interfaces/kf6_org.kde.KGlobalAccel.xml`,
  `kf6_org.kde.kglobalaccel.Component.xml`.
- Факты хоста: `plasmashell --version` → 6.7.5; `busctl --user status
  org.kde.kglobalaccel` → PID `kwin_wayland`;
  `pacman -Ql kglobalacceld` → `libKGlobalAccelD.so.0` слинкован в kwin.
