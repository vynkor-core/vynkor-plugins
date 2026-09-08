# Vynkor Device Protocol (VDP) — Спецификация

**Версия:** 1.0  
**Статус:** Draft  
**Дата:** 2026-09-07

## Обзор

Vynkor Device Protocol (VDP) — универсальный бинарный протокол для связи
устройств (ESP8266/ESP32, Zigbee, Thread) с ядром Vynkor.

**Цели:**
- Один формат сообщений для всех транспортов
- Бинарный (protobuf): компактный, быстрый парсинг
- Расширяемый: добавление полей не ломает старые устройства
- Multi-transport: WiFi/MQTT, Zigbee, Thread

## Свойства протокола

| Свойство | Значение |
|---|---|
| Кодировка | Protocol Buffers (proto3) |
| Размер поля version | 4 байта (major << 16 \| minor) |
| Макс. размер сообщения | 1024 байта (MQTT), 80 байт (Zigbee ZCL) |
| Endianness | Big-endian (network byte order) |
| Временные метки | Unix timestamp (UTC), секунды |
| Уникальность msg_id | UUID v4 или MAC+sequence |

## Структура сообщения

```
┌─────────────────────────────────────────────────┐
│                  Message                        │
├─────────────────────────────────────────────────┤
│ version: uint32       (4 bytes)                 │
│ device_id: string     (variable)                │
│ device_type: enum     (1 byte)                  │
│ timestamp: uint64     (8 bytes)                 │
│ uptime_s: uint32      (4 bytes)                 │
│ msg_id: string        (variable)                │
│ signal_strength: int32 (4 bytes)                │
│ battery_pct: int32    (4 bytes)                 │
│ payload_type: enum    (1 byte)                  │
│ payload: oneof        (variable)                │
└─────────────────────────────────────────────────┘
```

## Транспортные адаптеры

### 1. WiFi/MQTT

Основной транспорт. Полная поддержка всех возможностей протокола.

**Топики:**
```
vynkor/{device_id}/telemetry    — телеметрия (device → Vynkor)
vynkor/{device_id}/state        — состояние (retained)
vynkor/{device_id}/command      — команды (Vynkor → device)
vynkor/{device_id}/response     — ответы (device → Vynkor)
vynkor/{device_id}/status       — online/offline (LWT)
vynkor/{device_id}/ota          — OTA обновления
vynkor/{device_id}/discovery    — автообнаружение
```

**QoS:**
- Command/Response: QoS 1 (at least once)
- Telemetry: QoS 0 (at most once)
- State: QoS 1 + retained
- Status (LWT): QoS 1 + retained

**LWT (Last Will and Testament):**
```
Topic:   vynkor/{device_id}/status
Payload: {"online": false}
QoS:     1
Retain:  true
```

**Плюсы:**
- Полная поддержка protobuf
- Retained messages для состояний
- QoS для надёжной доставки
- LWT для online/offline статуса

### 2. Zigbee

Адаптация для Zigbee 3.0. Использует кастомный кластер.

**Кластер:** 0xFC00 (Vynkor Custom)

**Endpoint:** 1 (primary)

**Profile:** 0xFC00 (Vynkor)

**Формат кадра:**
```
┌─────────────────────────────────────────┐
│ ZCL Frame                               │
├─────────────────────────────────────────┤
│ Frame Control: 0x00 (cluster-specific)  │
│ Sequence: uint8                         │
│ Command ID: 0x01 (vynkor_message)       │
│ Payload: length-prefix + protobuf       │
└─────────────────────────────────────────┘
```

**Length-prefix:** 2 байта (big-endian), максимальное значение 80 байт.

**Ограничения:**
- Макс. размер payload: 80 байт (ZCL limitation)
- Нет retained messages
- Нет QoS ( delivery ack через ZCL)
- Discovery через Zigbee Device Profile (ZDP)

**Фрагментация:**
Для сообщений > 80 байт используется фрагментация:
```
┌─────────────────────────────────────────┐
│ Fragment Header                         │
├─────────────────────────────────────────┤
│ Total Size: uint16 (big-endian)         │
│ Fragment Index: uint8                   │
│ Fragment Count: uint8                   │
│ Data: bytes (up to 76 bytes)            │
└─────────────────────────────────────────┘
```

**Mapping команд:**
```yaml
# Zigbee → Vynkor command mapping
ZCL On/Off:      command: "set_relay", params: {"state": true/false}
ZCL Level:       command: "set_pwm", params: {"value": 0-255}
ZCL Temperature: command: "read_sensor", params: {"sensor": "temperature"}
ZCL Occupancy:   command: "read_sensor", params: {"sensor": "motion"}
```

### 3. Thread (Matter-compatible)

Адаптация для Thread mesh networks. Использует UDP.

**Порт:** 7890 (configurable)

**Формат кадра:**
```
┌─────────────────────────────────────────┐
│ UDP Payload                             │
├─────────────────────────────────────────┤
│ Magic: 0x56 0x59 0x4E 0x4B ("VYNK")    │
│ Version: uint8                          │
│ Length: uint16 (big-endian)              │
│ Protobuf: bytes                         │
│ CRC16: uint16 (big-endian)              │
└─────────────────────────────────────────┘
```

**Discovery:** mDNS/DNS-SD
```
Service Type: _vynkor._udp.local
Instance: {device_id}
Port: 7890
TXT Records:
  - type={device_type}
  - fw={firmware_version}
  - chip={chip_id}
```

**Matter Integration:**
Для полной совместимости с Matter/CHIP используется:
- Cluster: Vynkor Custom (0x131B)
- Command: vynkor_message (0x00)
- Payload: protobuf

## Версионирование

**Формат version:** `major << 16 | minor`

- **Major:** несовместимые изменения. Требуется обновление прошивки.
- **Minor:** новые поля/сообщения. Старые устройства продолжают работать.

**Примеры:**
- v1.0 → v1.1: добавлено новое поле в StatePayload (обратно совместимо)
- v1.0 → v2.0: изменён формат CommandPayload (потребуется прошивка)

**Политика совместимости:**
- Устройства с minor < текущего игнорируют новые поля
- Устройства с major < текущего отвечают ERROR_NOT_SUPPORTED
- Vynkor plugin поддерживает все major версии

## Безопасность

### Уровень 1: Транспорт

- **MQTT:** TLS 1.2+ (рекомендуется), username/password или certificate
- **Zigbee:** Network Key + Trust Center (стандарт Zigbee 3.0)
- **Thread:** Matter security (CASE/PASE)

### Уровень 2: Приложение

- **Device Authentication:** device_id + pre-shared key (PSK) или certificate
- **Message Integrity:** HMAC-SHA256 (опционально, для критических команд)
- **Encryption:** AES-128-GCM (опционально, для чувствительных данных)

### Уровень 3: Policy

- **Command Allowlist:** whitelisting команд в плагине
- **Rate Limiting:** ограничение частоты команд
- **Audit Log:** журнал всех команд в database plugin

## Discovery Protocol

Устройства сообщают о себе при подключении:

```
Device → Vynkor:
  Discovery {
    mac: "A1B2C3D4E5F6"
    supported_commands: ["set_relay", "read_sensor", "get_state"]
    pins: [
      {name: "GPIO5", type: DIGITAL, writable: true},
      {name: "DHT22", type: DIGITAL, writable: false}
    ]
    description: "Kitchen temperature sensor + relay"
  }

Vynkor → Device:
  (auto) registered in mqtt_device_list
```

## Примеры использования

### Пример 1: Датчик температуры

```
# Устройство публикует телеметрию каждые 30 сек
Topic:   vynkor/sensor-a1b2c3/telemetry
Payload: Message {
  version: 0x00010000,
  device_id: "sensor-a1b2c3",
  device_type: SENSOR,
  timestamp: 1725705600,
  payload_type: TELEMETRY,
  telemetry: {
    stream: "sensors",
    data: {
      "temperature": Value { float_val: 23.5 },
      "humidity": Value { float_val: 45.2 }
    }
  }
}
```

### Пример 2: Управление реле

```
# Vynkor отправляет команду
Topic:   vynkor/relay-kitchen/command
Payload: Message {
  version: 0x00010000,
  device_id: "relay-kitchen",
  device_type: ACTUATOR,
  timestamp: 1725705600,
  msg_id: "cmd-abc-123",
  payload_type: COMMAND,
  command: {
    command: "set_relay",
    params: {
      "pin": Value { int_val: 5 },
      "state": Value { bool_val: true }
    },
    timeout_ms: 5000
  }
}

# Устройство отвечает
Topic:   vynkor/relay-kitchen/response
Payload: Message {
  version: 0x00010000,
  device_id: "relay-kitchen",
  msg_id: "cmd-abc-123",  // тот же ID!
  payload_type: RESPONSE,
  response: {
    error_code: ERROR_OK,
    data: {
      "pin": Value { int_val: 5 },
      "state": Value { bool_val: true }
    },
    execution_ms: 12
  }
}
```

### Пример 3: Zigbee датчик движения

```
# Zigbee кластер 0xFC00, attribute 0x0001
ZCL Frame:
  Frame Control: 0x00
  Sequence: 0x42
  Command ID: 0x01
  Length: 0x002A (42 bytes)
  Protobuf: Message {
    version: 0x00010000,
    device_id: "motion-001122",
    device_type: SENSOR,
    timestamp: 1725705600,
    payload_type: TELEMETRY,
    telemetry: {
      stream: "motion",
      data: {
        "motion": Value { bool_val: true },
        "lux": Value { int_val: 340 }
      }
    }
  }
```

## Сравнение с альтернативами

| Протокол | Размер | Скорость | Расширяемость | Multi-transport |
|---|---|---|---|---|
| **VDP (protobuf)** | ~30-100 байт | Быстрый | Отлично | ✅ WiFi/Zigbee/Thread |
| JSON | ~100-500 байт | Медленный | Хорошо | ✅ WiFi |
| MQTT (raw) | varies | varies | Плохо | ✅ WiFi |
| ZCL (native) | ~20-50 байт | Быстрый | Плохо | ❌ Zigbee only |
| Matter | ~50-200 байт | Быстрый | Хорошо | ✅ WiFi/Thread |

**Почему protobuf:**
1. **Компактность:** 30-100 байт vs 100-500 байт JSON
2. **Скорость:** бинарный парсинг, без строковых операций
3. **Расширяемость:** field numbers, обратная совместимость
4. **Типизация:** строгая типизация, автогенерация кода
5. **Уже в vynkor:** proto файл используется в vynkor-wire

## Ограничения

1. **MQTT:** максимальный размер取决于 брокера (обычно 256KB - 1MB)
2. **Zigbee:** макс. 80 байт ZCL payload (фрагментация для больших сообщений)
3. **Thread:** UDP, ненадёжный (нужна транспортная репликация)
4. **ESP8266:** ограниченная память (~40KB free heap), protobuf解码占用 память

## ROADMAP

- [ ] v1.0: базовый протокол (telemetry, command, response, state)
- [ ] v1.1: OTA support
- [ ] v1.2: discovery protocol
- [ ] v2.0: encryption (AES-128-GCM)
- [ ] v2.1: group commands (multicast)
- [ ] v3.0: Matter integration
