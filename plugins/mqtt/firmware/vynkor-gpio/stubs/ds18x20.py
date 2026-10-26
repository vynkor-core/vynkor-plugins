ROMS = [b'\x28\x01\x02\x03\x04\x05\x06\x07']


class DS18X20:
    def __init__(self, ow):
        self.ow = ow
        self.converted = False

    def scan(self):
        return list(ROMS)

    def convert_temp(self):
        self.converted = True

    def read_temp(self, rom):
        if not self.converted:
            raise OSError('read before convert')
        return 19.25
