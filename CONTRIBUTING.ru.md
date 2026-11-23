# Участие в разработке podlib

Спасибо за интерес к развитию podlib!

Also available in English: [CONTRIBUTING.md](CONTRIBUTING.md).

## Настройка окружения

- Rust 1.95 или новее (через [rustup](https://rustup.rs)) — MSRV workspace'а
- C-компилятор (gcc/clang) — SQLite бандлится и собирается при сборке
- Для GUI: [ffmpeg](https://ffmpeg.org) (transcode) и desktop GL-стек
  (Wayland/X11) — есть на любом обычном десктопе

```sh
git clone https://github.com/apilot/podlib.git
cd podlib
cargo build
cargo test
```

## Перед открытием PR

- `cargo test` зелёный по всему workspace (unit + fixture-тесты)
- `cargo clippy --all-targets -- -D warnings` — без предупреждений
- Новое поведение сопровождается тестами; исправления багов — с падающим
  тестом-репродукцией, написанным первым
- Функции держим маленькими; всё, что знает формат БД устройства, живёт
  внутри адаптера бэкенда (`crates/podlib` → адаптер libopod)

## Работа с реальным устройством

- Запись на iPod всегда идёт через стейджинг edit-session
  (stage → install). Живую БД напрямую не мутировать никогда: прошивка
  ловит рассинхрон SQLite ↔ CDB ↔ CBK.
- Аппаратно-зависимые тесты закрыты гейтами (см.
  `crates/vendor/libopod/HARDWARE_TESTING.md`) — гейты не снимать и не
  обходить.
- Если можете проверить на живом железе (Nano 7G или классический iPod) —
  напишите об этом в PR, это сильно помогает.

## Vendored libopod

`crates/vendor/libopod` — запиненный форк
[koriwi/libopod](https://github.com/koriwi/libopod), опубликованный на
crates.io как [`podlib-libopod`](https://crates.io/crates/podlib-libopod)
(имя `libopod-rs` на crates.io принадлежит upstream). Политика:

- Без нужды её не править. Фиксы уровня БД идут upstream первыми — наш
  [koriwi/libopod#1](https://github.com/koriwi/libopod/pull/1) (NULL-safe
  rank-запросы, защита от orphan-артистов, reindex пустых ключей) — пример
  такого пути — и затем попадают в вендорку.
- Специфичное для podlib поведение (книжка edit-session, безопасное
  удаление artwork, playlist-мутации, поддержка classic) живёт в форке,
  пока upstream его не примет или не выйдет crates.io-релиз с этим; каждое
  опубликованное изменение форка поднимает версию форк-крейта.

## Сообщение о проблемах

Откройте [issue](https://github.com/apilot/podlib/issues) с указанием:
модель устройства и версия прошивки, версия podlib (git rev), что делали,
что ожидали, что получилось. Приложите вывод CLI/GUI. Свои дампы БД без
просьбы не прикладывайте.
