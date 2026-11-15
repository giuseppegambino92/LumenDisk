/*
 * lumendisk_ffi.h — C ABI of lumendisk-core (Rust) consumed by the Swift app.
 *
 * Conventions
 *  - Every function returning `char *` returns a heap-allocated, NUL-terminated UTF-8 JSON
 *    document that MUST be released with ld_free_string().
 *  - Every JSON reply has the shape {"ok": true, ...} or {"ok": false, "error": "<message>"}.
 *  - All functions are thread-safe and may block (they perform disk I/O / spawn mount_nfs);
 *    call them off the main thread.
 *
 * JSON types (field names are snake_case):
 *
 *  DirtyState   = {"state":"Clean"} | {"state":"Dirty","reason":"<text>"}
 *
 *  PartitionInfo = {
 *    "index": u32,              // 0 = whole device (no partition table), else 1-based
 *    "start": u64, "size": u64, // bytes
 *    "scheme": "GPT"|"MBR"|"NONE",
 *    "type_name": str, "type_id": str, "name": str,
 *    "fs_type": "EXT2"|"EXT3"|"EXT4"|"FAT12"|"FAT16"|"FAT32"|"EXFAT"|"NTFS"|null,
 *    "label": str|null, "uuid": str|null,
 *    "dirty": DirtyState|null,
 *    "total_bytes": u64|null, "free_bytes": u64|null
 *  }
 *
 *  MountRequest = {
 *    "path": str,                       // image file or /dev/diskNsM (or /dev/diskN + partition)
 *    "partition": u32,                  // 0 = whole device / superfloppy
 *    "mode": "readWrite"|"shadow"|"readOnly",
 *    "ghost_shield": bool,              // default true
 *    "uid_map": {"enabled": bool, "linux_uid": u32|null, "linux_gid": u32|null}, // null = auto-detect
 *    "mount_root": str|null,            // default: /Volumes if writable, else ~/Volumes
 *    "name": str|null,                  // mountpoint folder name, default: label
 *    "fd": i32|null                     // descriptor already authorised by the app (authopen)
 *  }
 *
 *  GhostRules = {"ds_store": bool, "apple_double": bool, "spotlight": bool,
 *                "trashes": bool, "fseventsd": bool, "other": bool}
 *
 *  VolumeStatus = {
 *    "id": str, "name": str, "source_path": str, "partition": u32,
 *    "fs_type": str, "label": str, "uuid": str,
 *    "mode": "readWrite"|"shadow"|"readOnly",
 *    "mountpoint": str, "port": u16,
 *    "read_only": bool, "forced_read_only_reason": str|null,
 *    "dirty": DirtyState,
 *    "total_bytes": u64, "free_bytes": u64,
 *    "ghost": {"enabled": bool, "blocked_count": u64, "vault_bytes": u64, "rules": GhostRules},
 *    "shadow": {"added": u64, "modified": u64, "deleted": u64, "pending_bytes": i64} | null,
 *    "uid_map": {"enabled": bool, "mac_uid": u32, "mac_gid": u32, "linux_uid": u32, "linux_gid": u32},
 *    "mounted_at": i64                   // unix seconds
 *  }
 *
 *  ShadowChange = {"path": str, "kind": "added"|"modified"|"deleted", "is_dir": bool,
 *                  "old_size": u64, "new_size": u64, "delta": i64}
 *
 *  GhostEntry   = {"path": str, "size": u64, "category": "ds_store"|"apple_double"|"spotlight"|"trashes"|"fseventsd"|"other"}
 *
 *  LockHolder   = {"pid": i32, "name": str, "executable": str|null, "files": [str]}
 *
 * ── Phase 2 additions ───────────────────────────────────────────────────────────────
 *
 *  MountRequest gains optional fields:
 *    "volume_icon": str|null      // path to an .icns shown as the Finder volume icon (virtualised
 *                                 // by Ghost Shield, never written to the disk)
 *
 *  VolumeStatus gains:
 *    "kind": "local"|"image_url"|"sftp"|"webdav"|"lan",
 *    "remote": RemoteStatus|null,
 *    "share":  ShareStatus|null,    // LAN share running for this volume
 *    "portal": PortalStatus|null,   // Wi-Fi drop portal running for this volume
 *    "journal": {"enabled": bool, "transactions": u64, "replayed_at_mount": u64}
 *
 *  RemoteStatus = {
 *    "url": str,                    // e.g. sftp://pi@raspberrypi.local/home/pi, https://…/raspios.img
 *    "online": bool,
 *    "latency_ms": u32|null,        // last successful round trip
 *    "pending_ops": u64,            // queued offline changes (files + structural ops)
 *    "pending_files": [str],        // up to 50 paths waiting to sync
 *    "last_sync": i64|null,         // unix seconds of last successful sync
 *    "last_error": str|null,
 *    "cache_bytes": u64,            // local metadata + read-ahead cache size
 *    "fetched_bytes": u64,          // bytes downloaded since mount
 *    "remote_size": u64|null        // image_url only: size of the remote image
 *  }
 *
 *  ShareStatus  = {"port": u16, "addresses": [str], "pairing_code": str, "service_name": str,
 *                  "clients": u32, "bytes_sent": u64, "bytes_received": u64}
 *  PortalStatus = {"url": str, "pin": str, "port": u16, "uploads": u64, "downloads": u64}
 *
 *  RemoteMountRequest = {
 *    "kind": "sftp"|"webdav"|"lan",
 *    "url": str,                    // sftp://user@host[:port]/path · http(s)://host/dav/path ·
 *                                   // http://host:port (lan, from Bonjour)
 *    "password": str|null,          // sftp password (else ssh-agent / keys) · webdav basic auth
 *    "identity_file": str|null,     // sftp private key
 *    "username": str|null,          // webdav basic auth user (overrides user@ in url)
 *    "pairing_code": str|null,      // lan: code shown on the sharing Mac
 *    "mode": "readWrite"|"shadow"|"readOnly",
 *    "ghost_shield": bool, "name": str|null, "mount_root": str|null, "volume_icon": str|null
 *  }
 *
 *  ImageUrlRequest = {
 *    "url": str,                    // http(s)://…/disk.img (server must support Range) or sftp://…/disk.img
 *    "password": str|null, "identity_file": str|null,
 *    "partition": u32,              // 0 = whole image
 *    "mode": "shadow"|"readOnly",   // writes always stay local (Shadow); the remote file is never modified
 *    "ghost_shield": bool, "name": str|null, "mount_root": str|null, "volume_icon": str|null
 *  }
 *
 * ── Phase 3 additions ───────────────────────────────────────────────────────────────
 *
 *  ShareStatus gains  "scheme": "https", "fingerprint": str   // SHA-256 of the TLS cert, hex lowercase;
 *                     publish it in the Bonjour TXT record as fp=<fingerprint>
 *  PortalStatus gains "url": https://… (encrypted, self-signed: phones show a one-time warning),
 *                     "http_url": str|null (unencrypted compatibility address), "fingerprint": str
 *  RemoteMountRequest gains "fingerprint": str|null  // lan: pin the share's certificate (from TXT fp=)
 *  VolumeStatus.journal gains "mode": "ordered", "wal_bytes": u64, "data_bytes": u64
 *  VolumeStatus gains "device": str|null   // BSD name ("disk4s1") when mounted from a physical device
 *
 *  BenchmarkResult = {"read_mbps": f64, "write_mbps": f64, "iops_4k": f64, "latency_ms": f64,
 *                     "bytes_tested": u64, "duration_ms": u64}
 *  DiskHealth = {"bsd_name": str, "media_name": str|null, "protocol": str|null,     // "USB", "Thunderbolt", "SATA", "PCI-Express", "Disk Image"
 *                "solid_state": bool|null, "smart_status": str|null,                  // "Verified" | "Failing" | "Not Supported" | null
 *                "size": u64|null, "removable": bool|null, "internal": bool|null,
 *                "location": str|null, "smartctl": object|null}                     // smartctl -j output when smartmontools is installed
 */
#ifndef LUMENDISK_FFI_H
#define LUMENDISK_FFI_H

#include <stdint.h>
#include <stdbool.h>

#ifdef __cplusplus
extern "C" {
#endif

/* Library version string, e.g. {"ok":true,"version":"1.0.0"}. */
char *ld_version(void);

/* Initialise the runtime. `support_dir` = ~/Library/Application Support/LumenDisk (created if
 * missing). Must be called once before anything else. Returns {"ok":...}. */
char *ld_init(const char *support_dir);

/* Inspect a source without mounting it: {"ok":true,"path":str,"size":u64,"partitions":[PartitionInfo]}.
 * request = {"path": str, "fd": i32|null} */
char *ld_inspect_source(const char *request_json);

/* Mount a volume (MountRequest). Reply: {"ok":true,"volume":VolumeStatus}. */
char *ld_mount(const char *request_json);

/* Unmount; `force` uses umount -f after flushing. Reply {"ok":...}. */
char *ld_unmount(const char *volume_id, bool force);

/* Unmount everything (Menu bar "Espelli Tutti"). Reply {"ok":true,"unmounted":u32,"failed":[str]}. */
char *ld_unmount_all(void);

/* {"ok":true,"volumes":[VolumeStatus]} */
char *ld_list_volumes(void);

/* Feature 2 — Shadow Mount. */
char *ld_shadow_changes(const char *volume_id);   /* {"ok":true,"changes":[ShadowChange]} */
char *ld_shadow_commit(const char *volume_id);    /* {"ok":true,"applied":u64} */
char *ld_shadow_rollback(const char *volume_id);  /* {"ok":true,"discarded":u64} */

/* Feature 1 — Ghost Shield. */
char *ld_ghost_entries(const char *volume_id);                       /* {"ok":true,"entries":[GhostEntry]} */
char *ld_ghost_set_rules(const char *volume_id, const char *rules);  /* rules = GhostRules JSON */
char *ld_ghost_set_enabled(const char *volume_id, bool enabled);
char *ld_ghost_purge(const char *volume_id);                         /* forget everything in the vault */

/* Feature 3 — UID/GID bridge: {"linux_uid":u32,"linux_gid":u32,"enabled":bool}. */
char *ld_set_uid_map(const char *volume_id, const char *map_json);

/* Feature 4 — Process Lock Inspector. */
char *ld_lock_inspect(const char *mountpoint);                        /* {"ok":true,"holders":[LockHolder]} */
char *ld_release_locks(const char *mountpoint, const char *pids_json); /* pids_json = [i32]; {"ok":true,"terminated":[i32]} */

/* Feature 6 — Dirty-bit & Journal Doctor.
 * ld_repair_source: repair an unmounted source. request = {"path":str,"partition":u32,"fd":i32|null}
 * ld_repair_volume: repair a mounted (forced read-only) volume and switch it to read/write.
 * Reply {"ok":true,"report":str}. */
char *ld_repair_source(const char *request_json);
char *ld_repair_volume(const char *volume_id);

/* Feature 5 — create a blank test image: {"path":str,"fs":"EXT4"|"NTFS"|"FAT32"|"EXFAT","size_mb":u32,"label":str}
 * Reply {"ok":true,"path":str}. */
char *ld_create_image(const char *request_json);

/* ── Phase 2: network & remote storage ─────────────────────────────────────────── */

/* Feature 1 — Zero-beachball remote mounts (RemoteMountRequest). {"ok":true,"volume":VolumeStatus} */
char *ld_mount_remote(const char *request_json);
/* Test a remote location without mounting: {"ok":true,"latency_ms":u32,"entries":u32} */
char *ld_probe_remote(const char *request_json);
/* Feature 5 — upload queued offline changes now: {"ok":true,"synced":u64,"pending":u64} */
char *ld_remote_sync_now(const char *volume_id);

/* Feature 2 — remote image streaming.
 * ld_inspect_image_url: {"ok":true,"path":url,"size":u64,"partitions":[PartitionInfo],"fetched_bytes":u64}
 * ld_mount_image_url:   {"ok":true,"volume":VolumeStatus} */
char *ld_inspect_image_url(const char *request_json);
char *ld_mount_image_url(const char *request_json);

/* Feature 3 — LAN share (WebDAV + bearer token derived from the pairing code). The app
 * publishes the Bonjour service `_lumendisk._tcp` itself (NetService) using ShareStatus.port
 * and TXT {"v":"1","name":...,"fs":...,"id":volume_id}.
 * ld_share_start: {"ok":true,"share":ShareStatus} */
char *ld_share_start(const char *volume_id);
char *ld_share_stop(const char *volume_id);

/* Feature 4 — "Drop-to-Disk" Wi-Fi portal (HTML upload/download, PIN protected).
 * ld_portal_start: {"ok":true,"portal":PortalStatus}  (encode PortalStatus.url in the QR code) */
char *ld_portal_start(const char *volume_id);
/* HTTPS only by default. enabled=1 also opens PortalStatus.http_url: an UNENCRYPTED address for
 * phones that refuse the self-signed certificate (PIN and files travel in clear); enabled=0
 * closes it again and ends the sessions created over it. {"ok":true,"portal":PortalStatus} */
char *ld_portal_set_plain_http(const char *volume_id, int enabled);
char *ld_portal_stop(const char *volume_id);

/* ── Phase 3 ──────────────────────────────────────────────────────────────────────── */

/* Real speed test through the mounted volume (≈3 s, writes then deletes a hidden temp file):
 * {"ok":true,"result":BenchmarkResult} */
char *ld_benchmark(const char *volume_id);

/* Hardware info / SMART status of a physical disk or partition ("disk4" or "disk4s1"):
 * {"ok":true,"health":DiskHealth} */
char *ld_disk_health(const char *bsd_name);

/* Language of user-visible core messages (errors, reasons): "it" (default) or "en".
 * {"ok":true,"language":"it"|"en"} */
char *ld_set_language(const char *code);

void ld_free_string(char *s);

#ifdef __cplusplus
}
#endif

#endif /* LUMENDISK_FFI_H */
