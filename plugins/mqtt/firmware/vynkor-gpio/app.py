import json
import machine
import network
import os
import sys
import time

import mqtt

VERSION = '0.3.0'
CONFIG_PATH = 'config.json'
OVERRIDE_PATH = 'overrides.json'
OVERRIDE_TMP = 'overrides.tmp'
OVERRIDE_MAX = 2048
# Overrides from 0.3.0 on hold the complete pin and sensor sets; older ones
# only add to config.json and must keep merging.
OVERRIDE_FORMAT = 2
LOG_PATH = 'log.txt'

COMMANDS = [
    'ping', 'get_state', 'get_pin', 'set_pin', 'toggle_pin',
    'set_pwm', 'read_sensor', 'config_get', 'config_set', 'reboot',
]

# GPIO wired to the SPI flash: touching one hangs or reboot-loops the chip.
FLASH_PINS = (6, 7, 8, 9, 10, 11)
# ESP32 GPIO without an output driver.
INPUT_ONLY = (34, 35, 36, 37, 38, 39) if sys.platform == 'esp32' else ()

WRITABLE = ('out', 'pwm')
DRIVERS = ('dht11', 'dht22', 'ds18b20')
DS18B20_CONVERT_MS = 750
# Without MQTT this long, reboot: a live loop that never reconnects is as
# dead as a hung one, and the hardware watchdog cannot see it.
OFFLINE_RESET_S = 600
# Every Nth failed join, restart the radio instead of only re-calling connect.
WIFI_RADIO_RESET_EVERY = 3


class UnknownCommand(Exception):
    code = 2


class UnknownPin(Exception):
    code = 3


class BadParams(Exception):
    code = 4


class HardwareError(Exception):
    code = 5


def log(msg):
    try:
        line = '[%d] %s\n' % (time.ticks_ms() // 1000, msg)
        with open(LOG_PATH, 'a') as f:
            f.write(line)
        if os.stat(LOG_PATH)[6] > 6000:
            with open(LOG_PATH) as f:
                tail = f.read()[-3000:]
            with open(LOG_PATH, 'w') as f:
                f.write(tail)
    except Exception:
        pass


def load_overrides():
    try:
        with open(OVERRIDE_PATH) as f:
            value = json.load(f)
        if isinstance(value, dict):
            return value
    except Exception:
        pass
    return None


def save_overrides(data):
    blob = json.dumps(data)
    if len(blob) > OVERRIDE_MAX:
        raise BadParams('config blob too large (%d bytes)' % len(blob))
    with open(OVERRIDE_TMP, 'w') as f:
        f.write(blob)
    try:
        os.remove(OVERRIDE_PATH)
    except OSError:
        pass
    os.rename(OVERRIDE_TMP, OVERRIDE_PATH)


def merge(base, over):
    out = dict(base)
    for key in over:
        if isinstance(over[key], dict) and isinstance(out.get(key), dict):
            out[key] = merge(out[key], over[key])
        else:
            out[key] = over[key]
    return out


def load_config():
    with open(CONFIG_PATH) as f:
        cfg = json.load(f)
    over = load_overrides()
    if not over:
        return cfg
    out = merge(cfg, over)
    if over.get('v') == OVERRIDE_FORMAT:
        # Complete sets replace config.json's, so a removal sticks.
        for key in ('pins', 'sensors'):
            if key in over:
                out[key] = over[key]
    return out


class Debouncer:
    """Turns raw digital reads into stable changes.

    update() is called every loop pass (~20 ms) with the raw level and the
    current ticks_ms(). It returns the new stable level when the input has
    changed and settled, or None otherwise.
    """

    def __init__(self, level, settle_ms):
        self.stable = level
        self.settle_ms = settle_ms
        self.candidate = level
        self.since = time.ticks_ms()

    def update(self, raw, now):
        # Every bounce restarts the wait: a level counts only after it has
        # held for settle_ms with no change at all.
        if raw != self.candidate:
            self.candidate = raw
            self.since = now
            return None
        if raw == self.stable or time.ticks_diff(now, self.since) < self.settle_ms:
            return None
        self.stable = raw
        return raw


class Sensor:
    """A driver sampled from the main loop without blocking it."""

    def __init__(self, name, spec, interval_ms):
        self.name = name
        self.spec = spec
        self.interval_ms = max(interval_ms, 2000)
        self.value = None
        self.error = ''
        self.at = None
        self.due = time.ticks_ms()

    def reading(self):
        out = {'driver': self.spec['driver'], 'value': self.value}
        if self.error:
            out['error'] = self.error
        if self.at is not None:
            out['age_s'] = time.ticks_diff(time.ticks_ms(), self.at) // 1000
        return out

    def store(self, value, error=''):
        self.value = value
        self.error = error
        self.at = time.ticks_ms()


class DhtSensor(Sensor):
    def __init__(self, name, spec, interval_ms):
        Sensor.__init__(self, name, spec, interval_ms)
        import dht
        cls = dht.DHT11 if spec['driver'] == 'dht11' else dht.DHT22
        self.dev = cls(machine.Pin(int(spec['pin'])))

    def poll(self, now):
        if time.ticks_diff(now, self.due) < 0:
            return False
        self.due = time.ticks_add(now, self.interval_ms)
        try:
            self.dev.measure()
            self.store({'temperature': self.dev.temperature(),
                        'humidity': self.dev.humidity()})
        except Exception as e:
            self.store(None, 'read failed: %s' % e)
        return True


class Ds18b20Sensor(Sensor):
    """Starts a conversion, comes back 750 ms later for the result."""

    def __init__(self, name, spec, interval_ms):
        Sensor.__init__(self, name, spec, interval_ms)
        import ds18x20
        import onewire
        self.dev = ds18x20.DS18X20(onewire.OneWire(machine.Pin(int(spec['pin']))))
        self.converting = None

    def poll(self, now):
        if self.converting is None:
            if time.ticks_diff(now, self.due) < 0:
                return False
            self.due = time.ticks_add(now, self.interval_ms)
            try:
                self.roms = self.dev.scan()
                if not self.roms:
                    self.store(None, 'no DS18B20 on the bus')
                    return True
                self.dev.convert_temp()
                self.converting = now
            except Exception as e:
                self.store(None, 'read failed: %s' % e)
                return True
            return False
        if time.ticks_diff(now, self.converting) < DS18B20_CONVERT_MS:
            return False
        self.converting = None
        try:
            temps = [self.dev.read_temp(rom) for rom in self.roms]
            self.store({'temperature': temps[0]} if len(temps) == 1
                       else {'temperature': temps})
        except Exception as e:
            self.store(None, 'read failed: %s' % e)
        return True


def make_sensor(name, spec, interval_ms):
    driver = spec.get('driver')
    if driver in ('dht11', 'dht22'):
        return DhtSensor(name, spec, interval_ms)
    if driver == 'ds18b20':
        return Ds18b20Sensor(name, spec, interval_ms)
    raise BadParams('unknown driver %s (have: %s)' % (driver, ', '.join(DRIVERS)))


class Agent:
    def __init__(self, cfg):
        self.cfg = cfg
        self.dev = cfg['device']
        self.base = 'vynkor/' + self.dev['id']
        self.pins = {}
        self.objs = {}
        self.inputs = {}
        self.sensors = {}
        self.client = None
        self.boot_ms = time.ticks_ms()
        self.last_telemetry = 0
        self.reboot_pending = False
        self.wdt = None
        self.online_at = self.boot_ms
        self.wifi_failures = 0

    def uptime(self):
        return time.ticks_diff(time.ticks_ms(), self.boot_ms) // 1000

    def wifi(self):
        return network.WLAN(network.STA_IF)

    def feed(self):
        if self.wdt is not None:
            self.wdt.feed()

    def nap(self, seconds):
        """time.sleep that keeps the ~2 s ESP8266 watchdog fed."""
        end = time.ticks_add(time.ticks_ms(), int(seconds * 1000))
        while time.ticks_diff(end, time.ticks_ms()) > 0:
            self.feed()
            time.sleep(0.1)

    def check_offline(self, now):
        limit = int(self.cfg.get('offline_reset_s', OFFLINE_RESET_S))
        if time.ticks_diff(now, self.online_at) > limit * 1000:
            log('offline for %ds, rebooting' % limit)
            machine.reset()

    def wifi_connect(self):
        w = self.cfg['wifi']
        sta = self.wifi()
        sta.active(True)
        try:
            network.hostname(self.dev['id'])
        except Exception:
            pass
        if sta.isconnected():
            self.wifi_failures = 0
            return True
        if self.wifi_failures and self.wifi_failures % WIFI_RADIO_RESET_EVERY == 0:
            log('wifi: restarting radio after %d failures' % self.wifi_failures)
            sta.disconnect()
            sta.active(False)
            self.nap(1)
            sta.active(True)
        sta.connect(w['ssid'], w['password'])
        for _ in range(200):
            if sta.isconnected():
                self.wifi_failures = 0
                return True
            self.nap(0.1)
        self.wifi_failures += 1
        return False

    def telemetry_ms(self):
        return int(self.cfg.get('telemetry_interval_s', 15)) * 1000

    def check_gpio(self, owner, num, mode):
        """Refuse GPIO that would brick the chip or is already taken."""
        if num in FLASH_PINS:
            raise BadParams('GPIO %d is wired to flash' % num)
        if mode in WRITABLE and num in INPUT_ONLY:
            raise BadParams('GPIO %d is input-only' % num)
        if mode == 'adc' and sys.platform == 'esp8266':
            if num != 0:
                raise BadParams('esp8266 has one ADC: use pin 0 (A0)')
            return  # A0 is not a GPIO, nothing to collide with
        for name in self.pins:
            if name != owner and int(self.pins[name]['pin']) == num and \
                    self.pins[name].get('mode') != 'adc':
                raise BadParams('GPIO %d is used by pin %s' % (num, name))
        for name in self.sensors:
            if name != owner and int(self.sensors[name].spec['pin']) == num:
                raise BadParams('GPIO %d is used by sensor %s' % (num, name))

    def setup_pin(self, name, spec, value=None):
        num = int(spec['pin'])
        mode = spec.get('mode', 'out')
        self.check_gpio(name, num, mode)
        self.release_pin(name)
        try:
            if mode == 'pwm':
                p = machine.PWM(machine.Pin(num))
                p.freq(int(spec.get('freq', 1000)))
                p.duty(int(value if value is not None else spec.get('default', 0)))
            elif mode == 'in':
                pull = machine.Pin.PULL_UP if spec.get('pull') == 'up' else None
                p = machine.Pin(num, machine.Pin.IN, pull)
            elif mode == 'adc':
                p = machine.ADC(0 if sys.platform == 'esp8266' else machine.Pin(num))
            elif mode == 'out':
                p = machine.Pin(num, machine.Pin.OUT)
            else:
                raise BadParams('unknown mode %s' % mode)
        except BadParams:
            raise
        except Exception as e:
            raise HardwareError('GPIO %d (%s): %s' % (num, mode, e))
        self.objs[name] = p
        self.pins[name] = spec
        if mode == 'in':
            self.inputs[name] = Debouncer(self.read_pin(name),
                                          int(spec.get('debounce_ms', 30)))
        if value is not None and mode == 'out':
            self.write_pin(name, value)

    def release_pin(self, name):
        obj = self.objs.pop(name, None)
        if obj is not None and self.pins[name].get('mode') == 'pwm':
            try:
                obj.deinit()
            except Exception:
                pass
        self.pins.pop(name, None)
        self.inputs.pop(name, None)

    def setup_sensor(self, name, spec):
        if 'pin' not in spec:
            raise BadParams('sensor %s needs a pin' % name)
        self.check_gpio(name, int(spec['pin']), 'sensor')
        if name in self.pins:
            raise BadParams('%s is already a pin' % name)
        try:
            self.sensors[name] = make_sensor(name, spec, self.telemetry_ms())
        except BadParams:
            raise
        except Exception as e:
            raise HardwareError('sensor %s: %s' % (name, e))

    def write_pin(self, name, value):
        spec = self.pins[name]
        mode = spec.get('mode', 'out')
        if mode not in WRITABLE:
            raise BadParams('pin %s is read-only (%s)' % (name, mode))
        if mode == 'pwm':
            duty = int(value)
            duty = 0 if duty < 0 else (1023 if duty > 1023 else duty)
            self.objs[name].duty(duty)
            return duty
        on = bool(value)
        if spec.get('active_low'):
            on = not on
        self.objs[name].value(1 if on else 0)
        return 1 if value else 0

    def read_pin(self, name):
        spec = self.pins[name]
        mode = spec.get('mode', 'out')
        if mode == 'pwm':
            return self.objs[name].duty()
        if mode == 'adc':
            return self.objs[name].read_u16()
        value = self.objs[name].value()
        if spec.get('active_low'):
            value = 0 if value else 1
        return value

    def resolve(self, params, create=False):
        name = params.get('name')
        if name and name in self.pins:
            return name
        if 'pin' in params:
            num = int(params['pin'])
            for existing in self.pins:
                if int(self.pins[existing]['pin']) == num:
                    return existing
            if create:
                name = name or 'pin%d' % num
                self.setup_pin(name, {
                    'pin': num,
                    'mode': 'out',
                    'active_low': bool(params.get('active_low', False)),
                }, params.get('value'))
                return name
        raise UnknownPin(str(params.get('name') or params.get('pin')))

    def state(self):
        sta = self.wifi()
        pins = {}
        for name in self.pins:
            pins[name] = self.read_pin(name)
        sensors = {}
        for name in self.sensors:
            sensors[name] = self.sensors[name].reading()
        return {
            'pins': pins,
            'sensors': sensors,
            'uptime_s': self.uptime(),
            'ip': sta.ifconfig()[0] if sta.isconnected() else '',
            'rssi': sta.status('rssi') if sta.isconnected() else 0,
        }

    def public_config(self):
        cfg = json.loads(json.dumps(self.cfg))
        if cfg.get('wifi'):
            cfg['wifi']['password'] = '***'
        if cfg.get('mqtt'):
            cfg['mqtt']['password'] = '***'
        return cfg

    def config_set(self, params):
        """Add, replace or (with null) remove pins and sensors; persist."""
        if 'telemetry_interval_s' in params:
            self.cfg['telemetry_interval_s'] = int(params['telemetry_interval_s'])
        for name, spec in (params.get('sensors') or {}).items():
            self.sensors.pop(name, None)
            if spec is not None:
                self.setup_sensor(name, spec)
        for name, spec in (params.get('pins') or {}).items():
            if spec is None:
                if name in self.pins:
                    self.release_pin(name)
            else:
                self.setup_pin(name, spec, spec.get('default'))
        pins = {}
        for name in self.pins:
            spec = self.pins[name]
            keep = {'pin': int(spec['pin']), 'mode': spec.get('mode', 'out')}
            for key in ('active_low', 'default', 'freq', 'pull', 'debounce_ms'):
                if key in spec:
                    keep[key] = spec[key]
            pins[name] = keep
        sensors = {}
        for name in self.sensors:
            spec = self.sensors[name].spec
            sensors[name] = {'driver': spec['driver'], 'pin': int(spec['pin'])}
        self.cfg['pins'] = pins
        self.cfg['sensors'] = sensors
        save_overrides({
            'v': OVERRIDE_FORMAT,
            'pins': pins,
            'sensors': sensors,
            'telemetry_interval_s': self.cfg.get('telemetry_interval_s', 15),
        })
        return {'pins': list(pins.keys()),
                'sensors': list(sensors.keys()),
                'telemetry_interval_s': self.cfg.get('telemetry_interval_s', 15)}

    def dispatch(self, command, params):
        if command == 'ping':
            return {'pong': True, 'uptime_s': self.uptime()}
        if command in ('get_state', 'get_pins'):
            return self.state()
        if command == 'get_pin':
            name = self.resolve(params)
            return {'name': name, 'value': self.read_pin(name)}
        if command == 'set_pin':
            name = self.resolve(params, create=True)
            return {'name': name, 'value': self.write_pin(name, params.get('value', 1))}
        if command == 'toggle_pin':
            name = self.resolve(params)
            current = self.read_pin(name)
            if self.pins[name].get('mode') == 'pwm':
                return {'name': name, 'value': self.write_pin(name, 0 if current else 1023)}
            return {'name': name, 'value': self.write_pin(name, 0 if current else 1)}
        if command == 'set_pwm':
            name = self.resolve(params)
            if self.pins[name].get('mode') == 'out':
                self.setup_pin(name, {
                    'pin': int(self.pins[name]['pin']),
                    'mode': 'pwm',
                    'freq': int(params.get('freq', 1000)),
                }, None)
            value = params.get('value', params.get('duty', 0))
            return {'name': name, 'value': self.write_pin(name, value)}
        if command == 'read_sensor':
            name = params.get('name') or params.get('sensor')
            if name not in self.sensors:
                raise UnknownPin(str(name))
            out = self.sensors[name].reading()
            out['name'] = name
            return out
        if command == 'config_get':
            return {'config': self.public_config()}
        if command == 'config_set':
            return self.config_set(params)
        if command == 'reboot':
            self.reboot_pending = True
            return {'rebooting': True}
        if command == 'ota_update':
            raise BadParams('ota_update is not supported by this firmware')
        raise UnknownCommand(command)

    def publish_response(self, msg_id, code, error, data, started):
        message = {
            'v': 1,
            'msg_id': msg_id,
            'error_code': code,
            'error': error,
            'data': data,
            'execution_ms': time.ticks_diff(time.ticks_ms(), started),
        }
        self.client.publish(self.base + '/response', json.dumps(message))

    def on_command(self, payload):
        started = time.ticks_ms()
        msg_id = None
        try:
            request = json.loads(payload)
            if not isinstance(request, dict):
                raise BadParams('command must be an object')
            msg_id = request.get('msg_id') or request.get('request_id')
            data = self.dispatch(request.get('command'), request.get('params') or {})
            self.publish_response(msg_id, 0, '', data, started)
        except UnknownCommand as e:
            self.publish_response(msg_id, e.code, 'unknown command: %s' % e, {}, started)
        except UnknownPin as e:
            self.publish_response(msg_id, e.code, 'unknown pin: %s' % e, {}, started)
        except BadParams as e:
            self.publish_response(msg_id, e.code, str(e), {}, started)
        except HardwareError as e:
            self.publish_response(msg_id, e.code, str(e), {}, started)
        except ValueError as e:
            self.publish_response(msg_id, 1, 'bad request: %s' % e, {}, started)
        except Exception as e:
            log('command: %s' % e)
            self.publish_response(msg_id, 6, 'internal: %s' % e, {}, started)
        if self.reboot_pending:
            time.sleep(0.3)
            machine.reset()

    def publish_status(self, online):
        sta = self.wifi()
        pins = []
        for name in self.pins:
            mode = self.pins[name].get('mode', 'out')
            pins.append({'name': name, 'pin_type': mode, 'writable': mode in WRITABLE})
        for name in self.sensors:
            pins.append({'name': name, 'pin_type': self.sensors[name].spec['driver'],
                         'writable': False})
        message = {
            'v': 1,
            'online': online,
            'device_id': self.dev['id'],
            'device_type': self.dev.get('type', 'actuator'),
            'firmware_name': self.dev.get('firmware', 'vynkor-gpio'),
            'firmware_version': VERSION,
            'chip_id': sys.platform,
            'ip_address': sta.ifconfig()[0] if sta.isconnected() else '',
            'wifi_rssi': sta.status('rssi') if sta.isconnected() else 0,
            'uptime_s': self.uptime(),
            'supported_commands': COMMANDS,
            'pins': pins,
        }
        self.client.publish(self.base + '/status', json.dumps(message), retain=True)

    def publish_stream(self, stream, data):
        message = {
            'v': 1,
            'stream': stream,
            'data': data,
            'uptime_s': self.uptime(),
        }
        self.client.publish(self.base + '/telemetry', json.dumps(message))

    def publish_telemetry(self):
        if self.pins:
            data = {}
            for name in self.pins:
                data[name] = self.read_pin(name)
            self.publish_stream('pins', data)
        if self.sensors:
            data = {}
            for name in self.sensors:
                data[name] = self.sensors[name].reading()
            self.publish_stream('sensors', data)

    def poll_inputs(self, now):
        """Debounced digital inputs; each settled change goes out at once."""
        changed = {}
        for name in self.inputs:
            level = self.inputs[name].update(self.read_pin(name), now)
            if level is not None:
                changed[name] = level
        if changed and self.client is not None:
            self.publish_stream('input', changed)

    def poll_sensors(self, now):
        for name in self.sensors:
            self.sensors[name].poll(now)

    def mqtt_connect(self):
        m = self.cfg['mqtt']
        will = json.dumps({
            'v': 1,
            'online': False,
            'device_id': self.dev['id'],
            'firmware_name': self.dev.get('firmware', 'vynkor-gpio'),
        })
        client = mqtt.MQTTClient(
            self.dev['id'], m['host'], m.get('port', 1883),
            m.get('user') or None, m.get('password') or None,
            m.get('keepalive', 30), self.base + '/status', will, self.feed)
        client.connect()
        client.subscribe(self.base + '/command')
        self.client = client
        self.last_telemetry = time.ticks_ms()
        self.publish_status(True)
        log('mqtt connected %s:%d' % (m['host'], m.get('port', 1883)))

    def setup(self):
        for name in self.cfg.get('pins', {}):
            try:
                self.setup_pin(name, self.cfg['pins'][name],
                               self.cfg['pins'][name].get('default', 0))
            except Exception as e:
                log('pin %s: %s' % (name, e))
        for name in self.cfg.get('sensors', {}):
            try:
                self.setup_sensor(name, self.cfg['sensors'][name])
            except Exception as e:
                log('sensor %s: %s' % (name, e))

    def run(self):
        self.setup()
        # Cannot be stopped once started; boot.py leaves a Ctrl-C window
        # before this for REPL access.
        self.wdt = machine.WDT()
        self.online_at = time.ticks_ms()
        last_err = None
        while True:
            try:
                self.feed()
                now = time.ticks_ms()
                self.check_offline(now)
                self.poll_sensors(now)
                if not self.wifi().isconnected():
                    if not self.wifi_connect():
                        if last_err != 'wifi down':
                            log('wifi down')
                            last_err = 'wifi down'
                        self.nap(2)
                        continue
                if self.client is None:
                    self.mqtt_connect()
                    last_err = None
                self.poll_inputs(now)
                message = self.client.check()
                self.online_at = time.ticks_ms()
                if message:
                    topic, payload = message
                    if topic == self.base + '/command':
                        self.on_command(payload)
                if time.ticks_diff(time.ticks_ms(), self.last_telemetry) > \
                        self.telemetry_ms():
                    self.last_telemetry = time.ticks_ms()
                    self.publish_telemetry()
            except Exception as e:
                text = 'loop: %s' % e
                if text != last_err:
                    log(text)
                    last_err = text
                if self.client is not None:
                    self.client.close()
                self.client = None
                self.nap(3)
            time.sleep(0.02)


def main():
    Agent(load_config()).run()
