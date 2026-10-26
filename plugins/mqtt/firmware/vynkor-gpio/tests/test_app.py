"""Host-side tests: app.py on CPython with the stubs in ../stubs.

    python3 -m unittest discover -s tests
"""

import json
import os
import shutil
import tempfile
import time
import types
import unittest

from support import clock  # sets import paths and the fake ticks

import app  # noqa: E402
import dht  # noqa: E402
import machine  # noqa: E402

BASE_CONFIG = {
    'wifi': {'ssid': 'x', 'password': 'x'},
    'mqtt': {'host': '127.0.0.1'},
    'device': {'id': 'dev'},
    'pins': {'led': {'pin': 2, 'mode': 'out', 'active_low': True}},
    'telemetry_interval_s': 15,
}


class FakeClient:
    def __init__(self):
        self.sent = []

    def publish(self, topic, payload, retain=False):
        self.sent.append((topic, json.loads(payload)))

    def streams(self, name):
        return [m['data'] for t, m in self.sent
                if t.endswith('/telemetry') and m['stream'] == name]

    def responses(self):
        return [m for t, m in self.sent if t.endswith('/response')]


class AgentTest(unittest.TestCase):
    def setUp(self):
        self.dir = tempfile.mkdtemp()
        self.cwd = os.getcwd()
        os.chdir(self.dir)
        with open('config.json', 'w') as f:
            json.dump(BASE_CONFIG, f)
        clock[0] = 0
        machine.levels.clear()
        dht.FAIL = False
        self.agent = app.Agent(app.load_config())
        self.agent.setup()
        self.agent.client = FakeClient()

    def tearDown(self):
        os.chdir(self.cwd)
        shutil.rmtree(self.dir)

    def command(self, command, **params):
        self.agent.on_command(json.dumps(
            {'msg_id': 'm', 'command': command, 'params': params}))
        return self.agent.client.responses()[-1]

    def tick(self, ms):
        clock[0] += ms
        self.agent.poll_inputs(clock[0])
        self.agent.poll_sensors(clock[0])


class Inputs(AgentTest):
    def setUp(self):
        AgentTest.setUp(self)
        machine.levels[4] = 1
        self.agent.setup_pin('button', {'pin': 4, 'mode': 'in', 'pull': 'up',
                                        'active_low': True, 'debounce_ms': 30})
        # Long idle first, so no test leans on a lockout left over from setup.
        self.tick(1000)

    def test_contact_bounce_yields_one_event(self):
        for level in (0, 1, 0, 1, 0):
            machine.levels[4] = level
            self.tick(5)
        for _ in range(10):
            self.tick(20)
        self.assertEqual(self.agent.client.streams('input'), [{'button': 1}])

    def test_glitch_shorter_than_debounce_is_ignored(self):
        machine.levels[4] = 0
        self.tick(10)
        machine.levels[4] = 1
        for _ in range(10):
            self.tick(20)
        self.assertEqual(self.agent.client.streams('input'), [])

    def test_press_and_release_are_two_events(self):
        machine.levels[4] = 0
        for _ in range(5):
            self.tick(20)
        machine.levels[4] = 1
        for _ in range(5):
            self.tick(20)
        self.assertEqual(self.agent.client.streams('input'),
                         [{'button': 1}, {'button': 0}])

    def test_input_is_read_only(self):
        reply = self.command('set_pin', name='button', value=1)
        self.assertEqual(reply['error_code'], 4)
        self.assertIn('read-only', reply['error'])

    def test_status_marks_input_not_writable(self):
        self.agent.publish_status(True)
        status = [m for t, m in self.agent.client.sent if t.endswith('/status')][-1]
        pins = {p['name']: p for p in status['pins']}
        self.assertFalse(pins['button']['writable'])
        self.assertTrue(pins['led']['writable'])
        self.assertEqual(status['firmware_version'], app.VERSION)


class Gpio(AgentTest):
    def test_flash_pins_are_refused(self):
        reply = self.command('set_pin', pin=6, value=1)
        self.assertEqual(reply['error_code'], 4)
        reply = self.command('config_set', pins={'bad': {'pin': 9, 'mode': 'out'}})
        self.assertEqual(reply['error_code'], 4)
        self.assertFalse(os.path.exists(app.OVERRIDE_PATH))

    def test_gpio_cannot_be_claimed_twice(self):
        reply = self.command('config_set', sensors={'t': {'driver': 'dht22', 'pin': 2}})
        self.assertEqual(reply['error_code'], 4)
        self.assertIn('used by pin led', reply['error'])

    def test_adc_reads_u16(self):
        machine.ADC.reading = 31000
        self.agent.setup_pin('light', {'pin': 32, 'mode': 'adc'})
        self.assertEqual(self.command('get_pin', name='light')['data']['value'], 31000)

    def test_esp8266_adc_is_a0_only(self):
        real = app.sys
        app.sys = types.SimpleNamespace(platform='esp8266')
        try:
            with self.assertRaises(app.BadParams):
                self.agent.setup_pin('light', {'pin': 5, 'mode': 'adc'})
            self.agent.setup_pin('light', {'pin': 0, 'mode': 'adc'})
        finally:
            app.sys = real


class Sensors(AgentTest):
    def test_dht_reading_and_failure(self):
        self.command('config_set', sensors={'room': {'driver': 'dht22', 'pin': 5}})
        self.tick(0)
        data = self.command('read_sensor', name='room')['data']
        self.assertEqual(data['value'], {'temperature': 21.5, 'humidity': 40.0})
        dht.FAIL = True
        self.tick(15000)
        data = self.command('read_sensor', name='room')['data']
        self.assertIsNone(data['value'])
        self.assertIn('read failed', data['error'])

    def test_ds18b20_does_not_block_the_loop(self):
        self.agent.setup_sensor('water', {'driver': 'ds18b20', 'pin': 13})
        self.tick(0)
        self.assertIsNone(self.agent.sensors['water'].value)
        self.tick(500)
        self.assertIsNone(self.agent.sensors['water'].value)
        self.tick(300)
        self.assertEqual(self.agent.sensors['water'].value, {'temperature': 19.25})

    def test_unknown_driver(self):
        reply = self.command('config_set', sensors={'x': {'driver': 'bme280', 'pin': 5}})
        self.assertEqual(reply['error_code'], 4)
        self.assertIn('unknown driver', reply['error'])


class Persistence(AgentTest):
    def test_config_survives_reboot_and_removal_sticks(self):
        self.command('config_set',
                     pins={'led': None, 'button': {'pin': 4, 'mode': 'in', 'pull': 'up'}},
                     sensors={'room': {'driver': 'dht11', 'pin': 5}})
        rebooted = app.Agent(app.load_config())
        rebooted.setup()
        self.assertEqual(sorted(rebooted.pins), ['button'])
        self.assertEqual(rebooted.pins['button']['pull'], 'up')
        self.assertEqual(sorted(rebooted.sensors), ['room'])

    def test_old_overrides_still_merge_with_config(self):
        # Written by 0.2.0: adds led_d1, says nothing about removing led.
        with open(app.OVERRIDE_PATH, 'w') as f:
            json.dump({'pins': {'led_d1': {'pin': 5, 'mode': 'out'}},
                       'telemetry_interval_s': 15}, f)
        self.assertEqual(sorted(app.load_config()['pins']), ['led', 'led_d1'])


class Watchdogs(AgentTest):
    def test_long_offline_reboots(self):
        self.agent.online_at = clock[0]
        self.agent.check_offline(clock[0] + app.OFFLINE_RESET_S * 1000)
        with self.assertRaises(SystemExit):
            self.agent.check_offline(clock[0] + app.OFFLINE_RESET_S * 1000 + 1)

    def test_nap_feeds_the_watchdog(self):
        self.agent.wdt = machine.WDT()
        before = machine.WDT.feeds
        real_sleep = time.sleep

        def fake_sleep(s):
            clock[0] += int(s * 1000)
        time.sleep = fake_sleep
        try:
            self.agent.nap(3)
        finally:
            time.sleep = real_sleep
        # Fed at least every 2 s (the ESP8266 limit is ~2.2 s).
        self.assertGreaterEqual(machine.WDT.feeds - before, 2)

    def test_radio_restarts_every_third_failed_join(self):
        events = []

        class DeadWifi:
            def active(self, on=None):
                if on is not None:
                    events.append('active %s' % on)

            def isconnected(self):
                return False

            def connect(self, ssid, password):
                events.append('connect')

            def disconnect(self):
                events.append('disconnect')

        dead = DeadWifi()
        self.agent.wifi = lambda: dead
        self.agent.nap = lambda s: None
        for _ in range(4):
            self.assertFalse(self.agent.wifi_connect())
        self.assertEqual(events.count('connect'), 4)
        self.assertEqual(events.count('disconnect'), 1)
        self.assertIn('active False', events)


if __name__ == '__main__':
    unittest.main()
