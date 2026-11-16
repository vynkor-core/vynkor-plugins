# Tests set FAIL to make measure() time out like an unplugged sensor.
FAIL = False


class DHTBase:
    def __init__(self, pin):
        self.pin = pin

    def measure(self):
        if FAIL:
            raise OSError(110)

    def temperature(self):
        return 21.5

    def humidity(self):
        return 40.0


class DHT11(DHTBase):
    pass


class DHT22(DHTBase):
    pass
