# podlib

**podlib** — свободная библиотека и консольная утилита для управления музыкальными библиотеками iPod по USB — современный Rust-взгляд на классический тандем libgpod + gtkpod.

Читается на английском: [README.md](README.md).

## Возможности

- Просмотр треков и плейлистов на устройстве
- Добавление, удаление и редактирование треков (название, исполнитель, альбом, жанр, номера трека/диска)
- Создание и удаление плейлистов, добавление и удаление треков из плейлистов
- Запись, безопасная для прошивки: изменения стейджатся и устанавливаются атомарно, контрольные файлы устройства (HASHAB/CBK) переподписываются автоматически

## Поддерживаемые устройства

| Устройство | Статус |
|---|---|
| iPod Nano 7-го поколения | ✅ Проверено на живом устройстве (прошивка 1.1.2) |
| Классические click-wheel iPod | Планируется (фаза 2) |

podlib построен на [libopod](https://github.com/koriwi/libopod) (vendored-копия, см. ниже) — первой библиотеке с поддержкой записи формата БД Nano 7G. Наши фиксы уровня БД (NULL-safe rank-запросы, защита от orphan-артистов при удалении, reindex пустых ключей) смержены upstream: [koriwi/libopod#1](https://github.com/koriwi/libopod/pull/1).

## Установка

Установка из репозитория через cargo (нужны Rust 1.95+ и C-компилятор):

```sh
cargo install --git https://github.com/apilot/podlib podlib-cli
# и/или GUI:
cargo install --git https://github.com/apilot/podlib podlib-gui
```

Готовые бинарники (`podlib` + `podlib-gui`, Linux x86_64/aarch64) также
прикладываются к [GitHub Releases](https://github.com/apilot/podlib/releases)
на каждый тег `v*` — скачайте tarball под свою платформу и распакуйте в
`~/.cargo/bin`.

`cargo binstall podlib-cli podlib-gui` заработает после публикации крейтов
на crates.io — сейчас блокировано: нужен релиз upstream libopod с поддержкой
плейлистов (vendored-копию опубликовать нельзя).

GUI дополнительно требует ffmpeg и desktop GL-стек в рантайме — см.
[Заметки по дистрибутивам](#заметки-по-дистрибутивам).

## Сборка

Требования:

- Rust 1.95 или новее (рекомендуется через [rustup](https://rustup.rs))
- C-компилятор (gcc/clang) — SQLite бандлится через libopod и собирается при сборке
- [ffmpeg](https://ffmpeg.org) — рантайм, используется GUI для transcode FLAC/OGG/WAV → AAC при добавлении
- GUI (`podlib-gui`) дополнительно требует desktop GL-стек (клиентские библиотеки Wayland или X11) — есть на любом обычном десктопе

```sh
git clone https://github.com/apilot/podlib.git
cd podlib
cargo build --release
```

Бинарник `podlib` (CLI) попадает в `target/release/`; `podlib-gui` — десктоп-приложение.

### Заметки по дистрибутивам

CLI требует только Rust-тулчейн — SQLite бандлится, системные библиотеки не нужны.

| Дистрибутив | Команда | Примечание |
|---|---|---|
| Arch | `pacman -S rust` | достаточно свежий |
| Fedora | `dnf install rust cargo` | достаточно свежий |
| Gentoo | `emerge --ask dev-lang/rust` | достаточно свежий |
| Debian / Ubuntu | используйте [rustup](https://rustup.rs) | пакеты дистрибутива часто старее требуемой 1.95 |

Дополнительно для GUI (ffmpeg для transcode; C-компилятор для бандленного SQLite):

| Дистрибутив | Команда |
|---|---|
| Arch | `pacman -S ffmpeg base-devel` |
| Fedora | `dnf install ffmpeg gcc` (ffmpeg через [RPM Fusion](https://rpmfusion.org)) |
| Gentoo | `emerge --ask media-video/ffmpeg` |
| Debian / Ubuntu | `sudo apt install ffmpeg build-essential` |

Монтирование из GUI использует `sudo`; для безопасного извлечения рекомендуется `udisksctl` (из пакета udisks2).

## Использование

```sh
podlib <MOUNT> list tracks
podlib <MOUNT> list playlists
podlib <MOUNT> playlist create "Road trip" 3220638850441492002
podlib <MOUNT> playlist add "Road trip" 4091926143709481856
podlib <MOUNT> track add --title "Song" --artist "Artist" --album "Album" /path/to/song.mp3
podlib <MOUNT> track remove 3220638850441492002 --purge
```

`<MOUNT>` — корень смонтированной файловой системы iPod (например, `/run/media/user/07516d6c-ab53-3096-a656-d954226a0226`). После работы всегда извлекайте устройство безопасно — см. ниже.

### Примечание про монтирование в Linux

Большинство десктоп-дистрибутивов (udisks 2.9+) монтируют HFS+-файловые системы iPod **только для чтения** по умолчанию. Для записи перемонтируйте том в read-write:

```sh
sudo mount -t hfsplus -o force,uid=$(id -u),gid=$(id -g),umask=22,nls=utf8 /dev/sdX2 /media/ipod
```

Грязный HFS+-journal блокирует монтирование на запись — сначала прогоните `fsck.hfsplus -f` по отмонтированному тому. Перед отключением отмонтируйте и выключите устройство:

```sh
sync
udisksctl unmount -b /dev/sdX2
udisksctl power-off -b /dev/sdX
```

GUI (`podlib-gui`) умеет подключение в один клик (этот mount-флоу автоматизирован внутри приложения).

## Как это устроено

- `crates/podlib` — библиотека: небольшая доменная модель (треки, плейлисты), трейт `LibraryBackend` и адаптер над [libopod](https://github.com/koriwi/libopod)
- `crates/podlib-cli` — консольная утилита `podlib`
- `crates/vendor/libopod` — запиненная копия libopod; её фиксы уровня БД смержены upstream ([koriwi/libopod#1](https://github.com/koriwi/libopod/pull/1)), специфичные для podlib фиксы edit-session живут локально до выхода crates.io-релиза с поддержкой плейлистов (см. [CONTRIBUTING.ru.md](CONTRIBUTING.ru.md))

Запись никогда не трогает живую БД напрямую: edit-session стейджится во временную директорию и устанавливается атомарно, а контрольные файлы Nano 7G (CBK) переподписываются FireWire GUID'ом устройства. Весь флоу проверен end-to-end на Nano 7G: плейлисты создаются и удаляются, треки добавляются и удаляются, прошивка после этого стартует штатно.

## Дорожная карта

1. ✅ Библиотека-ядро + CLI (iPod Nano 7G)
2. ✅ Бэкенд классических iPod (iTunesDB)
3. Smart-плейлисты, обложки, рейтинги и счётчики воспроизведения, импорт/экспорт, поиск осиротевших записей и дубликатов
4. ✅ Графический интерфейс (drag & drop, подключение в один клик)

## Лицензия

podlib — свободное ПО: вы можете распространять и/или изменять его на условиях [лицензии MIT](LICENSE).

Сторонние компоненты:

- [libopod](https://github.com/koriwi/libopod) (vendored-копия) — лицензия MIT; фиксы podlib приняты upstream ([koriwi/libopod#1](https://github.com/koriwi/libopod/pull/1))
- SQLite (бандлится через libopod) — public domain

podlib — независимая реализация, разработанная на основе публичных исследований форматов БД iPod. Она не содержит исходного кода Apple.

## Участие в разработке

Сообщения об ошибках, идеи и pull request'ы приветствуются в [issue-трекере](https://github.com/apilot/podlib/issues). Для крупных изменений сначала откройте issue и обсудите подход. Настройка окружения и конвенции — в [CONTRIBUTING.ru.md](CONTRIBUTING.ru.md): ожидаются unit-тесты (`cargo test`) и чистый `cargo clippy`.
