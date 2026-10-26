import socket
import time


def _rl(n):
    out = bytearray()
    while True:
        b = n & 0x7F
        n >>= 7
        if n:
            out.append(b | 0x80)
        else:
            out.append(b)
            return bytes(out)


def _str(s):
    b = s.encode()
    return bytes([len(b) >> 8, len(b) & 0xFF]) + b


class MQTTClient:
    def __init__(self, client_id, host, port=1883, user=None, password=None,
                 keepalive=30, will_topic=None, will_msg=None, idle=None):
        self.client_id = client_id
        self.host = host
        self.port = port
        self.user = user
        self.password = password
        self.keepalive = keepalive
        self.will_topic = will_topic
        self.will_msg = will_msg
        self.sock = None
        self.buf = b''
        self.pid = 0
        self.last_ping = 0
        self.last_rx = 0
        self.inbox = []
        # Called while connect() waits, so the caller can feed a watchdog.
        self.idle = idle or (lambda: None)

    def _connect_packet(self):
        flags = 0x02
        payload = _str(self.client_id)
        if self.will_topic:
            flags |= 0x04 | 0x20
            payload += _str(self.will_topic) + _str(self.will_msg or '')
        if self.user is not None:
            flags |= 0x80
            payload += _str(self.user)
            if self.password is not None:
                flags |= 0x40
                payload += _str(self.password)
        head = b'\x00\x04MQTT\x04' + bytes([flags]) + bytes(
            [self.keepalive >> 8, self.keepalive & 0xFF])
        body = head + payload
        return b'\x10' + _rl(len(body)) + body

    def connect(self, timeout=10, tcp_timeout=1.5):
        # tcp_timeout stays under the ESP8266 watchdog (~2.2 s): a blocking
        # connect cannot feed it. getaddrinfo blocks too, so use an IP.
        addr = socket.getaddrinfo(self.host, self.port)[0][-1]
        s = socket.socket()
        s.settimeout(tcp_timeout)
        try:
            s.connect(addr)
        except OSError:
            s.close()
            raise
        s.settimeout(0.05)
        self.sock = s
        self.buf = b''
        self.inbox = []
        self._send(self._connect_packet())

        started = time.ticks_ms()
        while time.ticks_diff(time.ticks_ms(), started) < timeout * 1000:
            self.idle()
            pkt = self._next()
            if pkt is None:
                self._fill()
                continue
            if pkt[0] & 0xF0 == 0x20:
                if len(pkt[1]) > 1 and pkt[1][1] == 0:
                    self.last_ping = self.last_rx = time.ticks_ms()
                    return True
                raise OSError('CONNACK refused: %d' % (pkt[1][1] if len(pkt[1]) > 1 else -1))
        raise OSError('CONNACK timeout')

    def _send(self, data):
        self.sock.sendall(data)

    def _fill(self):
        try:
            data = self.sock.recv(512)
        except OSError as e:
            if e.args and e.args[0] in (11, 110):  # EAGAIN, ETIMEDOUT: no data yet
                return False
            raise
        if not data:
            raise OSError('connection closed')
        self.buf += data
        self.last_rx = time.ticks_ms()
        return True

    def _next(self):
        b = self.buf
        if len(b) < 2:
            return None
        mult = 1
        rl = 0
        i = 1
        while True:
            if i >= len(b):
                return None
            d = b[i]
            rl += (d & 0x7F) * mult
            mult *= 128
            i += 1
            if not d & 0x80:
                break
            if mult > 2097152:
                raise OSError('bad remaining length')
        if len(b) < i + rl:
            return None
        body = b[i:i + rl]
        self.buf = b[i + rl:]
        return b[0], body

    def publish(self, topic, payload, retain=False):
        if isinstance(payload, str):
            payload = payload.encode()
        t = topic.encode()
        body = bytes([len(t) >> 8, len(t) & 0xFF]) + t + payload
        header = 0x31 if retain else 0x30
        self._send(bytes([header]) + _rl(len(body)) + body)

    def subscribe(self, topic):
        self.pid = (self.pid + 1) & 0xFFFF
        t = topic.encode()
        body = bytes([self.pid >> 8, self.pid & 0xFF,
                      len(t) >> 8, len(t) & 0xFF]) + t + b'\x00'
        self._send(b'\x82' + _rl(len(body)) + body)

    def ping(self):
        self._send(b'\xc0\x00')
        self.last_ping = time.ticks_ms()

    def check(self):
        """Read the socket; return one queued (topic, payload) or None.

        Several PUBLISHes can arrive in one recv: all are queued and handed
        out one per call, none is dropped.
        """
        self._fill()
        while True:
            pkt = self._next()
            if pkt is None:
                break
            header, body = pkt
            kind = header & 0xF0
            if kind == 0x30:
                qos = (header >> 1) & 3
                tlen = (body[0] << 8) | body[1]
                topic = body[2:2 + tlen].decode()
                off = 2 + tlen
                if qos > 0:
                    pid = (body[off] << 8) | body[off + 1]
                    off += 2
                    if qos == 1:
                        self._send(bytes([0x40, 0x02, pid >> 8, pid & 0xFF]))
                self.inbox.append((topic, body[off:]))
            elif kind == 0xE0:
                raise OSError('broker disconnected')
        now = time.ticks_ms()
        # We ping every keepalive/2, so silence past 1.5x keepalive means the
        # link is dead even if the socket has not noticed (half-open TCP).
        if time.ticks_diff(now, self.last_rx) > self.keepalive * 1500:
            raise OSError('keepalive timeout: broker silent')
        if time.ticks_diff(now, self.last_ping) > self.keepalive * 500:
            self.ping()
        return self.inbox.pop(0) if self.inbox else None

    def close(self):
        try:
            self.sock.close()
        except Exception:
            pass
        self.sock = None
