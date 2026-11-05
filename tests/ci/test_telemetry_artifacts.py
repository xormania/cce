"""Content identity, integrity and containment of captured artifact files."""
from __future__ import annotations

from dataclasses import replace
import errno
import hashlib
import os
from pathlib import Path
import stat
import sys
import tempfile
import unittest
from unittest import mock

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "tools/telemetry"))

from agent_telemetry.artifacts import FileArtifactStore  # noqa: E402
from agent_telemetry.domain import TelemetryError  # noqa: E402


class ArtifactStoreTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.base = Path(self.temporary.name)
        self.root = self.base / "objects"
        self.store = FileArtifactStore(self.root)

    def object_path(self, data):
        digest = hashlib.sha256(data).hexdigest()
        return self.root / "sha256" / digest[:2] / digest

    def captured_files(self):
        return sorted(path.relative_to(self.root).as_posix()
                      for path in self.root.rglob("*") if path.is_file())

    def test_captured_reference_matches_independent_content_digest_and_size(self):
        data = b"\x00binary evidence\xff\r\n" + "snow: \u96ea".encode("utf-8")
        ref = self.store.put(data, media_type="application/example")

        self.assertEqual(ref.digest, hashlib.sha256(data).hexdigest())
        self.assertEqual(ref.size_bytes, len(data))
        self.assertEqual(ref.media_type, "application/example")
        self.assertEqual(ref.availability, "captured")
        self.assertEqual(self.object_path(data).read_bytes(), data)
        self.assertTrue(self.store.verify(ref))
        self.assertEqual(self.store.read(ref), data)

    def test_empty_artifact_is_captured_with_its_real_identity(self):
        ref = self.store.put(b"")

        self.assertEqual(ref.digest, hashlib.sha256(b"").hexdigest())
        self.assertEqual(ref.size_bytes, 0)
        self.assertTrue(self.store.verify(ref))
        self.assertEqual(self.store.read(ref), b"")

    def test_identical_capture_reuses_one_content_object(self):
        data = b"same content on an uncertain retry"
        first = self.store.put(data, media_type="text/plain")
        second = self.store.put(data, media_type="application/octet-stream")

        self.assertEqual(first.digest, second.digest)
        self.assertEqual(first.size_bytes, second.size_bytes)
        self.assertEqual(len(self.captured_files()), 1)
        self.assertTrue(self.store.verify(first))
        self.assertTrue(self.store.verify(second))
        self.assertEqual(self.store.read(second), data)

    def test_distinct_contents_keep_independent_objects(self):
        first = self.store.put(b"first")
        second = self.store.put(b"second")

        self.assertNotEqual(first.digest, second.digest)
        self.assertEqual(len(self.captured_files()), 2)
        self.assertEqual(self.store.read(first), b"first")
        self.assertEqual(self.store.read(second), b"second")

    def test_reopened_store_reads_existing_content_without_rewriting_it(self):
        data = b"available after reopening"
        ref = self.store.put(data)
        before = self.object_path(data).stat().st_mtime_ns
        reopened = FileArtifactStore(self.root)

        self.assertTrue(reopened.verify(ref))
        self.assertEqual(reopened.read(ref), data)
        self.assertEqual(self.object_path(data).stat().st_mtime_ns, before)

    def test_temporary_object_sync_failure_never_advertises_captured_content(self):
        data = b"durable only after file synchronization"
        real_fsync = os.fsync
        failed_sizes = []

        def fail_object_sync(fd):
            info = os.fstat(fd)
            if stat.S_ISREG(info.st_mode):
                failed_sizes.append(info.st_size)
                raise OSError(errno.ENOSPC, "artifact storage full")
            return real_fsync(fd)

        with mock.patch("agent_telemetry.artifacts.os.fsync", side_effect=fail_object_sync):
            with self.assertRaises(TelemetryError):
                self.store.put(data)

        self.assertEqual(failed_sizes, [len(data)])
        self.assertFalse(self.object_path(data).exists())
        self.assertEqual(self.captured_files(), [])

    def test_failed_object_install_never_advertises_captured_content(self):
        data = b"complete temporary bytes before failed installation"
        attempted = []

        def fail_install(source, target, *, src_dir_fd, dst_dir_fd, follow_symlinks):
            fd = os.open(source, os.O_RDONLY, dir_fd=src_dir_fd)
            with os.fdopen(fd, "rb") as stream:
                attempted.append((target, stream.read()))
            raise OSError(errno.EIO, "artifact installation failed")

        with mock.patch("agent_telemetry.artifacts.os.link", side_effect=fail_install):
            with self.assertRaises(TelemetryError):
                self.store.put(data)

        self.assertEqual(attempted, [(hashlib.sha256(data).hexdigest(), data)])
        self.assertFalse(self.object_path(data).exists())
        self.assertEqual(self.captured_files(), [])

    def test_changed_bytes_fail_verification_and_cannot_be_read_as_captured(self):
        data = b"original"
        ref = self.store.put(data)
        self.object_path(data).write_bytes(b"replaced")

        self.assertFalse(self.store.verify(ref))
        with self.assertRaises(TelemetryError):
            self.store.read(ref)

    def test_truncated_bytes_fail_verification_and_cannot_be_read_as_captured(self):
        data = b"complete object"
        ref = self.store.put(data)
        self.object_path(data).write_bytes(data[:-1])

        self.assertFalse(self.store.verify(ref))
        with self.assertRaises(TelemetryError):
            self.store.read(ref)

    def test_reference_size_is_verified_alongside_digest(self):
        ref = self.store.put(b"payload")
        wrong_size = replace(ref, size_bytes=ref.size_bytes + 1)

        self.assertFalse(self.store.verify(wrong_size))
        with self.assertRaises(TelemetryError):
            self.store.read(wrong_size)

    def test_missing_captured_object_is_an_integrity_failure(self):
        data = b"captured before loss"
        ref = self.store.put(data)
        self.object_path(data).unlink()

        self.assertFalse(self.store.verify(ref))
        with self.assertRaises(TelemetryError):
            self.store.read(ref)

    def test_reported_orphans_are_not_deleted_or_rewritten(self):
        retained = self.store.put(b"referenced")
        orphan = self.store.put(b"installed before interrupted event append")
        before = {path: path.read_bytes() for path in self.root.rglob("*")
                  if path.is_file()}

        self.assertEqual(self.store.orphans({retained.digest}), [orphan.digest])
        self.assertEqual({path: path.read_bytes() for path in before}, before)
        self.assertEqual(self.captured_files(), sorted(
            path.relative_to(self.root).as_posix() for path in before))
        self.assertTrue(self.store.verify(retained))
        self.assertTrue(self.store.verify(orphan))

    def test_all_referenced_objects_leave_no_orphans(self):
        refs = [self.store.put(data) for data in (b"one", b"two")]

        self.assertEqual(self.store.orphans({ref.digest for ref in refs}), [])

    def test_symlink_store_root_does_not_redirect_capture(self):
        outside = self.base / "outside"
        outside.mkdir()
        redirected = self.base / "redirected"
        redirected.symlink_to(outside, target_is_directory=True)

        with self.assertRaises(TelemetryError):
            FileArtifactStore(redirected).put(b"must stay contained")
        self.assertEqual(list(outside.iterdir()), [])

    def test_symlink_hash_directory_does_not_redirect_capture(self):
        outside = self.base / "outside"
        outside.mkdir()
        self.root.mkdir(exist_ok=True)
        (self.root / "sha256").symlink_to(outside, target_is_directory=True)

        with self.assertRaises(TelemetryError):
            self.store.put(b"must stay contained")
        self.assertEqual(list(outside.iterdir()), [])

    def test_symlink_prefix_does_not_redirect_capture(self):
        data = b"must stay contained"
        outside = self.base / "outside"
        outside.mkdir()
        object_path = self.object_path(data)
        object_path.parent.parent.mkdir(parents=True, exist_ok=True)
        object_path.parent.symlink_to(outside, target_is_directory=True)

        with self.assertRaises(TelemetryError):
            self.store.put(data)
        self.assertEqual(list(outside.iterdir()), [])

    def test_symlink_object_is_not_reused_even_when_target_bytes_match(self):
        data = b"outside evidence"
        outside = self.base / "outside.bin"
        outside.write_bytes(data)
        object_path = self.object_path(data)
        object_path.parent.mkdir(parents=True, exist_ok=True)
        object_path.symlink_to(outside)

        with self.assertRaises(TelemetryError):
            self.store.put(data)
        self.assertEqual(outside.read_bytes(), data)
        self.assertTrue(object_path.is_symlink())

    def test_replaced_object_symlink_is_not_followed_on_read(self):
        data = b"outside evidence"
        ref = self.store.put(data)
        outside = self.base / "outside.bin"
        outside.write_bytes(data)
        object_path = self.object_path(data)
        object_path.unlink()
        object_path.symlink_to(outside)

        self.assertFalse(self.store.verify(ref))
        with self.assertRaises(TelemetryError):
            self.store.read(ref)
        self.assertEqual(outside.read_bytes(), data)


if __name__ == "__main__":
    unittest.main()
