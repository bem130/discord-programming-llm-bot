"""Trusted collector invoked after the sandbox control group has stopped."""
import base64
import json
import os
import stat
import sys

MAX_BYTES = 4 * 1024 * 1024
directory = os.open(sys.argv[1], os.O_DIRECTORY | os.O_NOFOLLOW)
artifacts = []
try:
    if int(sys.argv[2]) == 0:
        for name in sys.argv[3:]:
            if name != os.path.basename(name) or name in ("", ".", ".."):
                raise ValueError("invalid artifact name")
            descriptor = os.open(name, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK, dir_fd=directory)
            with os.fdopen(descriptor, "rb") as stream:
                info = os.fstat(stream.fileno())
                if not stat.S_ISREG(info.st_mode) or info.st_nlink != 1:
                    raise ValueError("artifact must be a regular file without hard links")
                if not 0 < info.st_size <= MAX_BYTES:
                    raise ValueError("artifact exceeds size limit")
                data = stream.read(MAX_BYTES + 1)
                if len(data) > MAX_BYTES or not data.startswith(b"\x89PNG\r\n\x1a\n"):
                    raise ValueError("artifact is not a bounded PNG")
                artifacts.append({"filename": name, "data": base64.b64encode(data).decode("ascii")})
    print("\nDISCORD_ARTIFACTS=" + json.dumps(artifacts), flush=True)
finally:
    os.close(directory)
