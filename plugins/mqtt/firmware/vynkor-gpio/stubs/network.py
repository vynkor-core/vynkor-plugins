STA_IF = 0


class WLAN:
    def __init__(self, iface=0):
        self._connected = True

    def active(self, v=None):
        return True

    def isconnected(self):
        return self._connected

    def connect(self, ssid, password):
        self._connected = True

    def ifconfig(self):
        return ('127.0.0.1', '255.0.0.0', '127.0.0.1', '127.0.0.1')

    def status(self, what):
        return -50


def hostname(name=None):
    return name or 'uesp12e-001'
