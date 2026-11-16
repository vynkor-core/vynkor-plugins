"""mqtt.py against a scripted socket.

    python3 -m unittest discover -s tests
"""

import time
import unittest

from support import clock  # sets import paths and the fake ticks

import mqtt  # noqa: E402


def publish(topic, payload):
    t = topic.encode()
    body = bytes([len(t) >> 8, len(t) & 0xFF]) + t + payload
    return b'\x30' + mqtt._rl(len(body)) + body


class FakeSocket:
    """recv() hands out the scripted chunks, then times out like a quiet link."""

    def __init__(self, chunks):
        self.chunks = list(chunks)
        self.sent = b''

    def recv(self, n):
        if not self.chunks:
            raise OSError(110)
        chunk = self.chunks.pop(0)
        if isinstance(chunk, Exception):
            raise chunk
        return chunk

    def sendall(self, data):
        self.sent += data

    def close(self):
        pass


def client(chunks, keepalive=10):
    c = mqtt.MQTTClient('dev', '127.0.0.1', keepalive=keepalive)
    c.sock = FakeSocket(chunks)
    clock[0] = 1000
    c.last_ping = c.last_rx = clock[0]
    return c


class Check(unittest.TestCase):
    def test_two_publishes_in_one_recv_are_both_delivered(self):
        c = client([publish('a/command', b'1') + publish('a/command', b'2')])
        self.assertEqual(c.check(), ('a/command', b'1'))
        self.assertEqual(c.check(), ('a/command', b'2'))
        self.assertIsNone(c.check())

    def test_publish_split_across_recvs_is_reassembled(self):
        packet = publish('a/command', b'hello')
        c = client([packet[:5], packet[5:]])
        self.assertIsNone(c.check())
        self.assertEqual(c.check(), ('a/command', b'hello'))

    def test_silent_broker_is_detected(self):
        c = client([])
        clock[0] += 14000
        c.check()  # within 1.5x keepalive: still fine, sends a ping
        self.assertIn(b'\xc0\x00', c.sock.sent)
        clock[0] += 2000
        with self.assertRaises(OSError) as raised:
            c.check()
        self.assertIn('keepalive timeout', str(raised.exception))

    def test_pingresp_keeps_the_link_alive(self):
        c = client([])
        for _ in range(10):
            clock[0] += 5000
            c.sock.chunks.append(b'\xd0\x00')  # PINGRESP
            c.check()

    def test_connection_reset_is_raised_not_swallowed(self):
        c = client([OSError(104)])
        with self.assertRaises(OSError):
            c.check()

    def test_closed_socket_is_raised(self):
        c = client([b''])
        with self.assertRaises(OSError):
            c.check()


if __name__ == '__main__':
    unittest.main()
