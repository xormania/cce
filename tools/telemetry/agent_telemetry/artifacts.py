"""Immutable content files installed before observation references."""

import hashlib
import os
from pathlib import Path
import re
import stat
import uuid
from .domain import ArtifactRef, TelemetryError

_FLAGS = os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC


def directory(path, create=False):
    path = Path(os.path.abspath(path))
    fd = os.open(path.anchor, _FLAGS)
    try:
        for part in path.parts[1:]:
            if create:
                try:
                    os.mkdir(part, mode=0o700, dir_fd=fd)
                    os.fsync(fd)
                except FileExistsError:
                    pass
            child = os.open(part, _FLAGS, dir_fd=fd)
            os.close(fd)
            fd = child
        return fd
    except BaseException:
        os.close(fd)
        raise


class FileArtifactStore:
    def __init__(self, root):
        self.root = Path(os.path.abspath(root))

    def _digest(self, ref):
        digest = ref.digest.removeprefix("sha256:")
        if not re.fullmatch(r"[0-9a-f]{64}", digest):
            raise TelemetryError("artifact_integrity", "invalid content digest")
        return digest

    def read(self, ref):
        digest = self._digest(ref)
        parent = None
        try:
            parent = directory(self.root / "sha256" / digest[:2])
            fd = os.open(digest, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK, dir_fd=parent)
            with os.fdopen(fd, "rb") as stream:
                info = os.fstat(stream.fileno())
                if not stat.S_ISREG(info.st_mode) or info.st_size != ref.size_bytes:
                    raise TelemetryError("artifact_integrity", "invalid artifact size or type")
                data = stream.read(ref.size_bytes + 1)
            if len(data) != ref.size_bytes or hashlib.sha256(data).hexdigest() != digest:
                raise TelemetryError("artifact_integrity", "artifact content differs")
            return data
        except OSError as error:
            raise TelemetryError("artifact_integrity", "artifact unavailable") from error
        finally:
            if parent is not None:
                os.close(parent)

    def verify(self, ref):
        try:
            self.read(ref)
            return True
        except TelemetryError:
            return False

    def put(self, data, media_type="application/octet-stream"):
        if not isinstance(data, bytes):
            raise TelemetryError("invalid_artifact", "artifact must be bytes")
        digest = hashlib.sha256(data).hexdigest()
        ref = ArtifactRef(digest, len(data), media_type)
        parent = None
        name = ".pending-" + uuid.uuid4().hex
        try:
            parent = directory(self.root / "sha256" / digest[:2], create=True)
            fd = os.open(name, os.O_CREAT | os.O_EXCL | os.O_WRONLY | os.O_NOFOLLOW, 0o600, dir_fd=parent)
            with os.fdopen(fd, "wb") as stream:
                stream.write(data)
                stream.flush()
                os.fsync(stream.fileno())
            try:
                os.link(name, digest, src_dir_fd=parent, dst_dir_fd=parent, follow_symlinks=False)
            except FileExistsError:
                if not self.verify(ref):
                    raise TelemetryError("artifact_integrity", "existing content object differs")
            os.unlink(name, dir_fd=parent)
            os.fsync(parent)
            return ref
        except OSError as error:
            raise TelemetryError("artifact_io", "artifact installation failed") from error
        finally:
            if parent is not None:
                try:
                    os.unlink(name, dir_fd=parent)
                except FileNotFoundError:
                    pass
                os.close(parent)

    def orphans(self, referenced_digests):
        known = {digest.removeprefix("sha256:") for digest in referenced_digests}
        # Report only. This never removes files or follows directory symlinks.
        found = []
        if not self.root.exists():
            return found
        rootfd = directory(self.root)
        os.close(rootfd)
        for root, dirs, files in os.walk(self.root, followlinks=False):
            dirs[:] = [name for name in dirs if not (Path(root) / name).is_symlink()]
            for name in files:
                if re.fullmatch(r"[0-9a-f]{64}", name) and name not in known:
                    found.append(name)
        return sorted(set(found))
