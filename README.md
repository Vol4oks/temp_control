# temp_control

Прошивка для **ESP32-C6** на Rust (`std` через `esp-idf-svc`): опрос датчика
DHT11, вывод показаний на OLED SSD1306 и двухточечная калибровка значений.
В разработке — веб-интерфейс с API и историей измерений в NVS.

## Текущее состояние

Реализовано (детали и план — в [docs/PLAN.md](docs/PLAN.md)):

- ✅ **Конфигурация** — `config.toml` (пины, интервалы, калибровка) и
  `secrets.toml` (Wi-Fi) запекаются в прошивку при сборке и парсятся при старте
- ✅ **`sensor_task`** — опрос DHT11 каждые 2 сек в отдельном потоке, отправка
  в mpsc-канал; ошибки чтения (таймаут/CRC) логируются, канал не засоряют
- ✅ **Двухточечная калибровка** — `real = k·measured + b` по двум парам
  «показал → реально» для T и H; в канал уходят и сырые, и откалиброванные
  значения (`raw_*`)
- ✅ **`display_task`** — SSD1306 по I2C (400 кГц), обновление раз в 1 сек:
  T/H крупным шрифтом + строка `IP: ---` внизу (реальный IP появится
  в Этапе 4 вместе с Wi-Fi)

В планах: Wi-Fi STA + HTTP-сервер (веб-UI, JSON API, браузерная калибровка
с сеткой) → история в NVS.

Открытые вопросы — [docs/QUESTIONS.md](docs/QUESTIONS.md).

## Железо

| Что | Пин | Примечание |
|-----|-----|------------|
| DHT11 DATA | GPIO4 | open-drain + подтяжка вверх (внутренняя; «голому» сенсору нужен внешний резистор 4.7–10 кОм) |
| SSD1306 SDA | GPIO6 | I2C, адрес 0x3C |
| SSD1306 SCL | GPIO7 | I2C, 400 кГц |

Дисплей 128×64. GPIO4–7 на ESP32-C6 — пины внешнего JTAG: с этой разводкой
внешний JTAG-отладчик работать не будет (прошивка через USB не страдает).

## Быстрый старт

### 1. Окружение

- Rust nightly с таргетом `riscv32imac-esp-espidf` и `rust-src`
  (ставится через [espup](https://github.com/esp-rs/espup): `cargo install espup && espup install`)
- [ldproxy](https://github.com/esp-rs/embuild): `cargo install ldproxy`
- Активация окружения перед сборкой (fish):

  ```fish
  source $HOME/export-esp.sh
  ```

ESP-IDF v5.5.3 скачивается автоматически при первой сборке в `.embuild/`
внутри проекта (`ESP_IDF_TOOLS_INSTALL_DIR = "workspace"`).

### 2. Секреты Wi-Fi

```fish
cp secrets.toml.example secrets.toml
```

и заполните `ssid` / `password`. Файл в `.gitignore`, в git не попадает.
Без `secrets.toml` сборка падает с подсказкой (проверка в `build.rs`).

### 3. Сборка и прошивка

```fish
cargo build          # первая сборка долгая: toolchain + ESP-IDF
cargo run            # = cargo espflash flash --monitor
```

Логи при старте: конфигурация, пины, интервалы, затем измерения
(`sensor_task: T=... (raw ...) H=...`) и кадры дисплея.

Проверки кода:

```fish
cargo check
cargo clippy --all-targets
cargo fmt --all -- --check
```

Таргет указывать не нужно — он задан в `.cargo/config.toml`.

При проблемах с зависимостями: `cargo clean && rm -f Cargo.lock`.

## Настройки (`config.toml`)

| Секция | Ключ | По умолчанию | Описание |
|--------|------|--------------|----------|
| `[pins]` | `dht11_data` | `4` | GPIO линии DATA DHT11 (0–30) |
| | `i2c_sda` / `i2c_scl` | `6` / `7` | GPIO шины I2C дисплея |
| `[display]` | `i2c_address` | `0x3C` | I2C-адрес SSD1306 |
| | `width` / `height` | `128` / `64` | разрешение; поддерживается только 128×64 |
| `[intervals]` | `sensor_poll_secs` | `2` | период опроса DHT11 (не чаще ~1 Гц) |
| | `display_refresh_secs` | `1` | период обновления экрана |
| | `history_save_secs` | `60` | период записи истории (Этап 5) |
| `[history]` | `max_points` | `240` | глубина истории в NVS (Этап 5) |
| `[calibration]` | `measured_1` / `real_1` | `0.0` / `0.0` | опорная точка 1: показал → реально |
| | `measured_2` / `real_2` | `1.0` / `1.0` | опорная точка 2 |

Секция `[calibration]` (подробности — ОВ-6 в docs/QUESTIONS.md):

- задаётся отдельно для `[calibration.temperature]` и `[calibration.humidity]`
- по умолчанию — идентичность (значения проходят без изменений); секцию
  можно удалить целиком
- показания пересчитываются по прямой через две точки; чистый сдвиг —
  частный случай (например, `0→2` и `10→12` даёт +2 °C)
- точки снимайте в заметно разных условиях: близкие `measured_*` дают
  нестабильный наклон
- сырые значения видны в логе и пойдут в API Этапа 4
- в Этапе 4 появится калибровка через браузер: тогда значения из браузера
  (NVS) будут иметь приоритет над `config.toml`

## Секреты (`secrets.toml`)

```toml
[wifi]
ssid = "MyNetwork"
password = "MyPassword"
```

⚠️ Пока Wi-Fi не реализован (Этап 4), значения не используются — но файл
нужен для сборки. Проверка `build.rs` падает с понятным сообщением, если
файла нет.

## Структура проекта

```
src/
├── main.rs          # запуск задач, раздача периферии и канала
├── config.rs        # парсинг config.toml + secrets.toml (serde)
├── pins.rs          # номера GPIO из конфига → handle'ы (единственный unsafe)
├── sensor.rs        # sensor_task: опрос DHT11, mpsc
├── calibration.rs   # двухточечная калибровка (валидация + пересчёт)
└── display.rs       # display_task: SSD1306 через embedded-graphics
```

Канал данных: `sensor_task` → `mpsc::channel` → `display_task` (позже —
веб-сервер и NVS-логгер). Калибровка — общее состояние
`Arc<Mutex<Calibration>>`: сенсор читает, веб-задача (Этап 4) будет менять.

## Документация

- [AGENTS.md](AGENTS.md) — правила работы AI-агента и архитектура проекта
- [docs/PLAN.md](docs/PLAN.md) — план работ и прогресс по этапам
- [docs/QUESTIONS.md](docs/QUESTIONS.md) — открытые вопросы и решения
