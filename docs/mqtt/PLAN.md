# MQTT Plugin — План реализации

**Статус:** Draft  
**Дата:** 2026-09-07  
**Ветка:** `feat/mqtt-plugin`

## Цель

Создать MQTT-плагин для Vynkor, который позволяет:
1. Управлять ESP-устройствами через MQTT
2. Получать телеметрию от датчиков
3. Выполнять OTA-обновления
4. Автообнаруживать устройства в сети

Плюс библиотека для ESP (Arduino/PlatformIO), которая реализует
протокол VDP на стороне устройства.

## Архитектура

```
┌─────────────────────────────────────────────────────────┐
│                    Vynkor (хост)                        │
│                                                         │
│  ┌─────────────┐     ┌──────────────┐                  │
│  │ mqtt plugin │────▶│ MQTT Broker  │◀──── TLS ────┐  │
│  └──────┬──────┘     └──────┬───────┘              │  │
│         │                   │                      │  │
│         │   ┌───────────────┼───────────────┐      │  │
│         │   │               │               │      │  │
│         ▼   ▼               ▼               ▼      │  │
│  ┌─────────────┐  ┌─────────────┐  ┌─────────────┐│  │
│  │ ESP Kitchen │  │ ESP Room    │  │ Zigbee GW   ││  │
│  │ (WiFi)      │  │ (WiFi)      │  │ (Zigbee→MQTT│◀┘  │
│  └─────────────┘  └─────────────┘  └─────────────┘    │
└─────────────────────────────────────────────────────────┘
```

## Фазы реализации

### Фаза 1: Протокол + Базовый плагин (MVP)

**Цель:** Работающий плагин, который подключается к MQTT и
обменивается сообщениями с ESP.

**Срок:** 3-4 дня

#### Шаг 1.1: Протокол (DONE)
- [x] `plugins/mqtt/proto/vynkor_mqtt.proto` — protobuf schema
- [x] `docs/mqtt/PROTOCOL.md` — спецификация
- [ ] Генерация Rust кода из proto

#### Шаг 1.2: Структура плагина
- [ ] `plugins/mqtt/Cargo.toml` — зависимости
- [ ] `plugins/mqtt/plugin.json` — манифест
- [ ] `plugins/mqtt/src/error.rs` — MqttError enum
- [ ] `plugins/mqtt/src/client.rs` — MQTT клиент обёртка (rumqttc)
- [ ] `plugins/mqtt/src/actions.rs` — обработчики действий
- [ ] `plugins/mqtt/src/stream.rs` — менеджер подписок (R6 streaming)
- [ ] `plugins/mqtt/src/lib.rs` — MqttPlugin struct + Plugin impl
- [ ] `plugins/mqtt/src/main.rs` — entry point

#### Шаг 1.3: Действия плагина
- [ ] `mqtt_connect` — подключение к MQTT брокеру
- [ ] `mqtt_disconnect` — отключение
- [ ] `mqtt_publish` — публикация сообщения (protobuf)
- [ ] `mqtt_subscribe` — подписка на топик (R6 streaming)
- [ ] `mqtt_unsubscribe` — отписка
- [ ] `mqtt_status` — статус соединения

#### Шаг 1.4: Тесты
- [ ] Unit tests: парсинг параметров, protobuf encode/decode
- [ ] Fake-kernel tests: registration + action dispatch
- [ ] Integration: Mosquitto + ESP-имитатор

**Делегирование:**
- Протокол:已完成 (я написал)
- Структура плагина: `task(category="deep", ...)`
- Тесты: `task(category="quick", ...)`

---

### Фаза 2: Устройства + Телеметрия

**Цель:** Автообнаружение устройств, получение телеметрии,
отправка команд.

**Срок:** 2-3 дня (после фазы 1)

#### Шаг 2.1: Discovery
- [ ] Обработка DiscoveryPayload (регистрация устройства)
- [ ] `mqtt_device_list` — список устройств
- [ ] `mqtt_device_info` — информация об устройстве
- [ ] Хранение устройств в database plugin

#### Шаг 2.2: Команды
- [ ] `mqtt_device_command` — отправка команды
- [ ] Корреляция request/response через msg_id
- [ ] Таймауты и retry

#### Шаг 2.3: Телеметрия
- [ ] Приём TelemetryPayload
- [ ] Хранение в database (per-device)
- [ ] `mqtt_device_telemetry` — запрос телеметрии
- [ ] Публикация событий `plugin.mqtt.telemetry`

**Делегирование:**
- Discovery + команды: `task(category="deep", ...)`
- Телеметрия: `task(category="deep", ...)`

---

### Фаза 3: OTA + Zigbee/Thread

**Цель:** OTA-обновления, поддержка Zigbee и Thread.

**Срок:** 3-4 дня (после фазы 2)

#### Шаг 3.1: OTA
- [ ] `mqtt_ota_update` — запуск OTA
- [ ] Чанкирование прошивки
- [ ] CRC32 проверка
- [ ] Прогресс через streaming

#### Шаг 3.2: Zigbee adapter
- [ ] Zigbee кластер 0xFC00
- [ ] Фрагментация (>80 байт)
- [ ] Mapping ZCL commands → VDP

#### Шаг 3.3: Thread adapter
- [ ] UDP transport (порт 7890)
- [ ] mDNS discovery
- [ ] Matter cluster mapping

**Делегирование:**
- OTA: `task(category="deep", ...)`
- Zigbee: `task(category="deep", ...)` (maybe research first)
- Thread: `task(category="deep", ...)`

---

### Фаза 4: ESP Библиотека

**Цель:** C++ библиотека для ESP8266/ESP32 (Arduino/PlatformIO).

**Срок:** 4-5 дней (параллельно с фазами 2-3)

#### Шаг 4.1: Ядро библиотеки
- [ ] `Vynkor.h` — основной класс
- [ ] MQTT клиент (PubSubClient)
- [ ] Protobuf encode/decode (nanopb)
- [ ] Авто-реконнект

#### Шаг 4.2: API
- [ ] `device.connectWiFi(ssid, pass)`
- [ ] `device.connectMQTT(host, port)`
- [ ] `device.registerSensor(name, callback)`
- [ ] `device.onCommand(name, handler)`
- [ ] `device.setTelemetryInterval(ms)`
- [ ] `device.enableOTA()`

#### Шаг 4.3: Конфигурационный портал
- [ ] AP mode при первом запуске
- [ ] Веб-интерфейс для настройки WiFi/MQTT
- [ ] Сохранение в EEPROM/SPIFFS

#### Шаг 4.4: Примеры
- [ ] `examples/temperature_sensor/` — датчик температуры
- [ ] `examples/relay_control/` — управление реле
- [ ] `examples/custom_firmware/` — пользовательская прошивка

**Делегирование:**
- Ядро библиотеки: `task(category="deep", ...)`
- Примеры: `task(category="quick", ...)`

---

## Dependencies

```
Фаза 1 (MVP)
  └── протокол (done)
  └── rumqttc (MQTT client)
  └── prost (protobuf)
  └── vynkor-sdk, vynkor-wire

Фаза 2 (Устройства)
  └── Фаза 1
  └── database plugin (хранение)

Фаза 3 (OTA/Zigbee/Thread)
  └── Фаза 2
  └── esptool (OTA на хосте)
  └── zigbee2mqtt (Zigbee, опционально)

Фаза 4 (ESP библиотека)
  └── Протокол (done)
  └── nanopb (protobuf для ESP)
  └── PubSubClient (MQTT для ESP)
```

## Risk Matrix

| Риск | Вероятность | Влияние | Mitigation |
|---|---|---|---|
| MQTT broker недоступен | Средняя | Высокое | Auto-reconnect, exponential backoff |
| Zigbee фрагментация сложна | Средняя | Среднее | Начать с WiFi, Zigbee позже |
| ESP8266 не влезает в protobuf | Низкая | Среднее | nanopb, minimal features |
| Thread не совместим с Matter | Низкая | Низкое | Thread как отдельный adapter |
| Пользователь не может прошить ESP | Средняя | Высокое | Веб-интерфейс (ESP Web Tools) |

## Success Criteria

### MVP (Фаза 1)
- [ ] Плагин подключается к MQTT брокеру
- [ ] Плагин отправляет/принимает protobuf сообщения
- [ ] ESP-имитатор работает через Mosquitto
- [ ] Тесты проходят

### Release (Фаза 2)
- [ ] Устройства автообнаруживаются
- [ ] Команды отправляются и получают ответы
- [ ] Телеметрия хранится и запрашивается
- [ ] Голосовое управление работает ("включи свет")

### Full (Фаза 3-4)
- [ ] OTA обновления работают
- [ ] Zigbee устройства поддерживаются
- [ ] ESP библиотека доступна в PlatformIO
- [ ] Примеры работают из коробки

## Out of Scope (MVP)

- Multi-broker (именованные профили)
- Web UI для управления
- Zigbee/Thread (фаза 3)
- Пользовательские команды через UI
- Групповые команды (multicast)

## Notes

- Протокол уже написан (vynkor_mqtt.proto)
- Спецификация протокола готова (PROTOCOL.md)
- Ветка создана: `feat/mqtt-plugin`
- Следующий шаг: структура плагина + MVP
