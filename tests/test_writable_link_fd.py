import tempfile,unittest
from pathlib import Path
from writable_link_fd import exercise

class NativeControl(unittest.TestCase):
    def test_ordinary_native_file_descriptor_remains_valid_across_hardlink(self):
        with tempfile.TemporaryDirectory() as root:
            events=[];exercise(Path(root),events)
            self.assertEqual(len(events),3)

if __name__=='__main__':unittest.main()
