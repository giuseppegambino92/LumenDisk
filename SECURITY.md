# Security

**English** | [Italiano](#italiano)

## Reporting a vulnerability

Please report security issues privately to the repository owner through GitHub (Security → Report a vulnerability) rather than in a public issue.

## Threat model

LumenDisk serves disks to macOS through network protocols that were not designed for a multi-user machine. This section lists what each component trusts, what it defends against, and what it does not.

### 1. Loopback NFS server (every mounted volume)

Each volume gets its own NFSv3 server bound to `127.0.0.1` on an ephemeral port, mounted by macOS's own `mount_nfs`.

**Adversary:** another local user, or a process of another user, on the same Mac, who wants to read or write a disk that you mounted. Remote hosts can't connect at all: the server listens on loopback only.

**Why this needs care:** NFSv3 uses `AUTH_SYS`. The client simply *states* a uid, and nothing is cryptographically authenticated. Any process that can open a TCP connection to the port could claim to be you.

**Defenses, in order of importance:**

1. **Fail-closed connection filter** (`peer_is_trusted` in `core/src/nfs_loopback.rs`). Every new TCP connection is checked against the system socket table. It is accepted only if its client socket is positively identified as one of these:
   - the kernel (`kernel_task`). This is the macOS NFS client. It performs the MOUNT handshake and all NFS traffic, and stamps each call with the real uid of the process doing the file operation.
   - a process of your own user, or of root. Both can already open the image or device directly, so they gain nothing.

   Everything else is refused. That includes the case where the owner can't be determined: `netstat` fails, or the row isn't found after retries. Refused connections are logged.
2. **AUTH_SYS uid check.** Calls must carry your uid or 0. This is meaningful *because* of (1): only the kernel and your own processes can reach the server, and the kernel doesn't let other users forge your uid.
3. **Single MOUNT.** The first MOUNT handshake (the one from `mount_nfs`) is the only one ever answered. If anything else won the race, LumenDisk's own mount would fail and the server would be torn down.
4. **128-bit secret in every file handle**, and a **128-bit random export path**. Handles can't be guessed or forged; a forged handle is rejected as stale. The export path is visible to other users while `mount_nfs` runs (it appears in `ps`), so it is not relied upon.
5. **The mount point** is created in your home folder (`~/Volumes`) with your permissions.

**Platform constraint (development builds):** macOS shows the system socket table only to processes signed with a **Developer ID**. Ad-hoc and Apple Development builds get an empty table, both from `netstat` and from the `pcblist` sysctls, so they can't verify anything. Such builds refuse to start a server with a clear error, unless `LUMENDISK_ALLOW_UNVERIFIED_LOOPBACK=1` is set:
- The tests set it (`.cargo/config.toml` and `scripts/build_and_verify.sh`).
- Developers can set it at their own risk.
- Official releases never need it.

`scripts/release.sh` proves this for every release: it mounts, writes and unmounts a volume with the notarized build and the variable unset. That only succeeds if the kernel's NFS client is positively identified.

**Out of scope:** root, and processes running as you. Either can already read your disks and your memory. Also out of scope: a compromised kernel, and physical access.

### 2. Wi-Fi portal ("Send & Receive over Wi-Fi")

A browser page, opened from a phone, for uploading and downloading files on a volume. It is PIN-protected and served on the local network.

**Adversary:** anyone on the same network, including people on public or shared Wi-Fi.

- **HTTPS only by default.** The server uses TLS with an ephemeral self-signed certificate generated at each start, so phones show a one-time certificate warning.
- **6-digit PIN**, rate limited (5 failures per minute per IP) and checked in constant time. Sessions last one hour and use the `__Host-`, `Secure`, `HttpOnly` and `SameSite=Strict` cookie attributes.
- **Unencrypted HTTP is off by default** and nothing listens on it. A phone that refuses the certificate can be served over plain HTTP only after an explicit confirmation that explains the risk. While HTTP is on:
  - anyone on the network can read the PIN, the file list and the files, and can tamper with them;
  - the window shows a persistent "insecure mode" warning.

  Switching HTTP off closes the port immediately and ends every session created over HTTP. Sessions are bound to the scheme they were created on, so an id that travelled in clear never opens the HTTPS side.
- The portal stops when you stop it or eject the volume.

### 3. LAN share (another Mac)

- WebDAV over **TLS 1.3**, with an ephemeral certificate. The certificate's SHA-256 fingerprint is published over Bonjour and **pinned** by the client.
- A per-share pairing code is turned into an HMAC proof bound to that fingerprint, so a man in the middle can't relay it.
- There is no plain-text mode.

### 4. Remote storage (SFTP, WebDAV, HTTP images)

- **SFTP** uses the system `ssh` with your configuration and `known_hosts`.
- **WebDAV and HTTP** use the system trust store over HTTPS. Redirects are not followed, so credentials are only ever sent to the configured server. The one exception is the usual "add a trailing slash" retry, which goes to the same URL.
- **Remote disk images** are opened copy-on-write: the remote file is never written.

### 5. Untrusted volumes

A USB stick or image can be malformed on purpose. All four drivers (ext4, NTFS, FAT, exFAT), the partition table parser and the journal replay treat on-disk data as untrusted:
- sizes and offsets are bounds-checked;
- allocations are capped by what the volume can physically hold;
- cycles in chains, trees and directories are detected;
- a corrupt structure returns an error instead of panicking or looping.

A panic in a driver is contained: the call returns an I/O error and the volume is marked failed, without taking down the app. This is exercised on every build by a deterministic corrupted-image suite. There is also continuous fuzzing with `cargo fuzz` (`scripts/fuzz.sh`); see the Verification section of the README.

### 6. Shadow Mode staging

Pending Shadow changes are stored in `~/Library/Application Support/LumenDisk/Shadow`, with your user's permissions. They are as private as the rest of your home folder. They are not encrypted beyond FileVault.

Sessions of SFTP, WebDAV and LAN volumes are named after a hash of the server's canonical address (protocol, host, port, path and user name; for a LAN share, the shared volume's identity). Passwords, pairing codes and tokens are never written there: the address is stored without them.

---

## Italiano

### Segnalare una vulnerabilità

Segnala i problemi di sicurezza in privato al proprietario del repository tramite GitHub (Security → Report a vulnerability), non in una issue pubblica.

### In breve

- **Server NFS locale.** Ascolta solo su `127.0.0.1`. Ogni connessione viene verificata con la tabella dei socket di sistema ed è accettata solo se proviene dal kernel (il client NFS di macOS), dal tuo utente o da root. Tutto il resto viene rifiutato, anche quando il proprietario non si riesce a identificare (fail-closed). `AUTH_SYS` non è un'autenticazione crittografica: diventa affidabile solo grazie a questo filtro. A questo si aggiungono:
  - un solo handshake MOUNT;
  - un segreto di 128 bit in ogni file handle;
  - un percorso di export casuale di 128 bit.
- **Build di sviluppo.** macOS mostra la tabella dei socket solo alle app firmate con un Developer ID. Le build ad-hoc quindi rifiutano di montare, a meno di impostare esplicitamente `LUMENDISK_ALLOW_UNVERIFIED_LOOPBACK=1`. `scripts/release.sh` verifica a ogni rilascio che la build notarizzata monti senza questa variabile.
- **Portale Wi-Fi.** Solo HTTPS per impostazione predefinita, con PIN a tentativi limitati e cookie `__Host-`/`Secure`. L'indirizzo HTTP non cifrato non è in ascolto finché non lo attivi esplicitamente, dopo un avviso che spiega che PIN e file viaggerebbero in chiaro. Quando lo disattivi, la porta si chiude e le sessioni HTTP terminano.
- **Condivisione tra Mac.** Solo TLS 1.3, con certificato fissato tramite Bonjour e codice di abbinamento legato al certificato.
- **Sessioni Shadow.** Le modifiche in sospeso restano in `~/Library/Application Support/LumenDisk/Shadow`, con i permessi del tuo utente (cifrate solo da FileVault). Le sessioni dei volumi SFTP, WebDAV e LAN prendono il nome da un hash dell'indirizzo del server; password, codici di abbinamento e token non vengono mai salvati.
- **Dischi non affidabili.** Driver con controlli sui limiti, rilevamento dei cicli, allocazioni limitate e isolamento dei panic. Ogni build esegue una suite deterministica di immagini corrotte, con in più fuzzing tramite `cargo fuzz`.
