# LumenDisk

[English](README.md) | **Italiano**

<a href="https://paypal.me/giuseppegambino92"><img src="https://www.paypalobjects.com/it_IT/IT/i/btn/btn_donateCC_LG.gif" alt="Fai una donazione con PayPal"></a>

**I dischi Linux e Windows sul tuo Mac, in lettura e scrittura, nel Finder. Senza macFUSE, senza estensioni del kernel, senza toccare la sicurezza dalla modalità di recupero.**

![macOS 14+](https://img.shields.io/badge/macOS-14%2B-black?logo=apple)
![Universal](https://img.shields.io/badge/Universal-Apple%20Silicon%20%2B%20Intel-blue)
![Notarizzata](https://img.shields.io/badge/notarizzata-Apple-brightgreen?logo=apple)
![Rust](https://img.shields.io/badge/core-Rust-orange?logo=rust)
![Swift 6](https://img.shields.io/badge/app-Swift%206-F05138?logo=swift)
![ext4 NTFS FAT32 exFAT](https://img.shields.io/badge/ext2%2F3%2F4%20·%20NTFS%20·%20FAT%20·%20exFAT-lettura%20%2B%20scrittura-success)

<p align="center">
  <img src="docs/screenshots/it-01-volume.png" alt="LumenDisk: un volume ext4 con la scheda Salute e Velocità Disco" width="860">
</p>

LumenDisk monta su macOS dischi, schede SD e immagini disco **ext2/ext3/ext4, NTFS, FAT12/16/32 ed exFAT** in lettura e scrittura completa, così si comportano come qualsiasi altro disco nel Finder. Monta anche **server SFTP, condivisioni WebDAV/NAS, altri Mac della tua rete** e **file `.img` / `.iso` remoti** senza scaricarli.

Tutto gira in spazio utente: un motore in Rust, con i suoi driver dei file system, presenta ogni volume a macOS tramite un server NFS privato in loopback, e il `mount_nfs` di macOS lo mette nel Finder. Nel kernel non si installa nulla e la Protezione dell'integrità del sistema resta attiva.

## Download

Scarica l'ultima versione da **[Releases](../../releases)**:

| File | Per |
|---|---|
| **LumenDisk-1.0.0-universal.dmg** | **qualsiasi Mac**, Apple Silicon e Intel (consigliato) |
| LumenDisk-1.0.0-arm64.dmg | solo Apple Silicon (M1, M2, M3, M4…) |
| LumenDisk-1.0.0-x86_64.dmg | solo Intel |
| LumenDisk-1.0.0-universal.zip | l'app universale, in formato zip |
| lumendisk-cli-1.0.0-macos-universal.tar.gz | lo strumento da riga di comando `lumendisk` |

Apri il DMG e trascina **LumenDisk** su **Applicazioni**. L'app è firmata con un Developer ID e notarizzata da Apple, quindi si apre come qualsiasi altra app. I checksum sono in `SHA256SUMS.txt`.

Richiede macOS 14 Sonoma o successivo.

## Cosa fa

### Dischi e immagini
- **Lettura e scrittura di ext2, ext3, ext4, NTFS, FAT12/16/32 ed exFAT**, da USB, Thunderbolt e SD oppure da immagini disco (`.img`, `.raw`, `.iso`, `.ext4`, `.ntfs`, `.fat32`).
- **Immagini con più partizioni**: trascina un'immagine Raspberry Pi e monti `bootfs` (FAT32) e `rootfs` (ext4) affiancate, pronte da modificare prima di scriverle sulla scheda.
- **I dischi che macOS non sa leggere** compaiono come *"Non montato da macOS · Pronto per LumenDisk"* con il pulsante **Monta Ora**.
- **Dischi di prova**: crea con un clic un'immagine vuota ext4, NTFS, FAT32 o exFAT.

### Protezione
- **Ghost Shield**: `.DS_Store`, i file AppleDouble `._*`, `.Spotlight-V100`, `.Trashes` e `.fseventsd` restano in un archivio locale e non vengono mai scritti sul disco Linux o Windows. Il Finder funziona come sempre.
- **Shadow Mode**: ogni modifica resta sul Mac. Controlli l'elenco dei file aggiunti, modificati ed eliminati, poi **Applica su Disco** oppure **Scarta Modifiche** e il disco resta intatto. Le modifiche in sospeso vengono registrate in un journal sul Mac, quindi sopravvivono a un crash, a un'uscita forzata, a un blackout o alla chiusura dell'app: la volta dopo che colleghi il disco LumenDisk ti propone di riprenderle. Se l'applicazione si interrompe (disco scollegato, disco pieno, crash), riparte da dove si era fermata; non viene annullata. Se nel frattempo il disco è stato modificato altrove, le modifiche salvate si possono solo scartare. Sui volumi SFTP, WebDAV e LAN funziona allo stesso modo: la sessione è legata all'indirizzo del server (mai a una password) e torna quando ti ricolleghi in Shadow Mode; **Applica su Disco** finisce solo quando il server ha ricevuto tutte le modifiche, e se sul server è cambiato qualcosa che la sessione aveva letto, le modifiche si possono solo scartare.
- **Niente di confermato va perso**: in lettura/scrittura e in Shadow, LumenDisk conferma una modifica a macOS solo quando è nel suo journal sul Mac. Se LumenDisk va in crash, viene chiuso forzatamente o terminato, non si perde nulla di confermato: nessuna cartella o file creato, rinominato o eliminato, nessun file chiuso o salvato con `fsync`. In caso di blackout sono al sicuro i file chiusi o salvati con `fsync` (contenuto, creazione e rinomine, `F_FULLFSYNC` prima che close() ritorni) e ogni altra modifica più vecchia di circa 100 ms.
- **Scritture a prova di blackout**: un journal ordinato dei metadati (come `data=ordered` di ext4). Se il Mac si spegne a metà copia, il disco viene riparato da solo al montaggio successivo. Le copie grandi vengono scritte una volta sola (0,05 % di journal in più su ext4).
- **Ponte dei permessi Linux**: i file del tuo utente Linux (per esempio `1000:1000`) sul Mac risultano tuoi, e i file nuovi vengono salvati con il proprietario Linux.
- **Riparazione flag "dirty" e journal**: riconosce i volumi lasciati in sospeso da Windows o Linux (flag dirty, ibernazione, journal ext4 non applicato) e li ripara con un clic.
- **Chi blocca il disco?**: mostra le app che tengono occupato un volume e lo espelle in sicurezza.

### Rete e archivi remoti
- **SFTP, WebDAV e NAS** che non bloccano mai il Finder: quando cade la rete, cartelle e file già aperti arrivano dalla cache locale in meno di un millisecondo, le scritture vanno in coda e tutto si sincronizza al ritorno della connessione, anche dopo aver chiuso l'app.
- **Immagini disco remote** via HTTP o SFTP: un'immagine Raspberry Pi da 600 MB si monta in meno di mezzo secondo scaricandone meno dell'1 %. Le modifiche restano sul Mac e il file remoto non viene mai toccato.
- **Condividi un disco con un altro Mac** della rete con un clic: scoperta con Bonjour, codice di abbinamento, cifratura TLS 1.3 con certificato verificato.
- **Ricevi e invia via Wi-Fi**: inquadra un codice QR con qualsiasi telefono per caricare o scaricare file direttamente sul disco esterno, protetto da PIN.

### Strumenti
- **Salute e Velocità Disco**: velocità reale di lettura, scrittura e accesso casuale 4K in circa 3 secondi, con un giudizio chiaro ("Veloce per video 4K"), più stato SMART, tipo di collegamento e SSD/HDD.
- **Icona nella barra dei menu** con tutti i dischi, il contatore di Ghost Shield, l'apertura nel Finder, la condivisione via QR e **Espelli Tutti in Sicurezza**.
- **Regole automatiche** per ogni disco: lettura/scrittura, sola lettura o Shadow, Ghost Shield e mappatura UID.
- **Interfaccia in italiano e inglese**, che puoi cambiare in qualsiasi momento.

## Screenshot

| | |
|---|---|
| ![Volume](docs/screenshots/it-01-volume.png) | ![Chiavetta USB pronta per LumenDisk](docs/screenshots/it-02-usb.png) |
| **Volume**: capacità, test di velocità, modalità di montaggio, Ghost Shield | **Chiavetta USB** che macOS non legge, pronta da montare |
| ![Disco di rete](docs/screenshots/it-03-network.png) | ![Shadow Staging](docs/screenshots/it-04-shadow.png) |
| **Disco di rete** con stato della connessione e coda offline | **Shadow Staging**: controlla le modifiche, poi Applica o Scarta |
| ![Ghost Shield](docs/screenshots/it-05-ghost.png) | ![Portale Wi-Fi con QR](docs/screenshots/it-06-wifi-qr.png) |
| **Ghost Shield**: i file di sistema di macOS restano fuori dal disco | **Ricevi/Invia via Wi-Fi**: codice QR e PIN |
| ![Immagini disco](docs/screenshots/it-07-images.png) | ![Barra dei menu](docs/screenshots/it-08-menubar.png) |
| **Immagini disco**: trascina, da URL, crea un disco di prova | **Barra dei menu** |

Tutti gli screenshot vengono dal motore reale con volumi reali (vedi [Verifica](#verifica)).

## Compilare dai sorgenti

Servono Xcode con Swift 6 e [rustup](https://rustup.rs). I target macOS `aarch64` e `x86_64` vengono aggiunti in automatico.

```sh
scripts/build_and_verify.sh                                 # compila ed esegue tutti i test
INSTALL=1 scripts/build_and_verify.sh                       # installa anche la build di sviluppo in /Applications
SKIP_E2E=1 SKIP_UI=1 SKIP_FUZZ=1 scripts/build_and_verify.sh  # solo compilazione + test unitari e immagini corrotte
```

Lo script produce `dist/LumenDisk.app` (universale, firma ad-hoc) e `build/bin/lumendisk` (riga di comando). I test end-to-end richiedono Docker; la verifica dell'interfaccia apre finestre dell'app.

Questa build di sviluppo ha una firma ad-hoc. macOS mostra la tabella dei socket di sistema solo alle app firmate con un Developer ID, quindi una build così non può controllare chi si collega al suo server NFS locale. Per questo rifiuta di montare, a meno di impostare `LUMENDISK_ALLOW_UNVERIFIED_LOOPBACK=1`; gli script di test lo impostano da soli. Vedi [SECURITY.md](SECURITY.md).

Per pubblicare, `scripts/release.sh` firma quel pacchetto con un Developer ID, lo fa notarizzare e prepara in `dist/release/` i DMG, lo zip e l'archivio della riga di comando. Usa l'account Apple collegato a Xcode, quindi non serve configurare certificati o password.

Lo strumento da riga di comando crea, esamina e monta immagini:

```sh
lumendisk mkfs ext4 prova.img 512 MioDisco
lumendisk inspect raspios.img
lumendisk mount raspios.img --partition 2 --mode shadow
```

### Fuzzing

Una chiavetta USB o un'immagine possono essere costruite apposta per essere malformate, quindi ogni volume è trattato come un input non fidato. `scripts/fuzz.sh [secondi per target]` esegue i target libFuzzer guidati dalla copertura (`core/fuzz`: ext4, NTFS, FAT, exFAT e il parser MBR/GPT). Ogni input è un'immagine intera che viene montata in sola lettura e in lettura/scrittura, esplorata, modificata e riparata. I semi sono immagini create dal mkfs di LumenDisk e, se Docker è disponibile, da `mkfs.ext4 -d`, `mkfs.vfat` + `mcopy`, `mkntfs` + `ntfscp` e `mkfs.exfat`. Servono una toolchain nightly e cargo-fuzz (`rustup toolchain install nightly`, `cargo install cargo-fuzz`). Se mancano, lo script lo segnala e si salta. `build_and_verify.sh` lo esegue per 30 s per target quando sono installati (`SKIP_FUZZ=1` lo salta). I crash vengono salvati in `core/fuzz/artifacts/`. Prima di correggerne uno, `cargo run --profile robust --example fuzz_seeds -- core/tests/corrupt_regressions --regression <target> <file>` lo trasforma in un caso di regressione minimizzato (pochi byte modificati in un'immagine seme riproducibile) che gira a ogni `cargo test`.

## Come funziona

```
Finder / qualsiasi app
      │  client NFS di macOS (presente in ogni Mac)
      ▼
mount_nfs 127.0.0.1:/<export casuale>  →  ~/Volumes/<Nome>
      │
      ▼  server NFSv3 privato in loopback, uno per volume
┌──────────────────────────── lumendisk-core (Rust) ────────────────────────────┐
│ Ghost Shield → Shadow → ponte UID → journal ordinato → ext4 / NTFS / FAT / exFAT │
│                                     └→ oppure un archivio remoto: SFTP · WebDAV · LAN │
└────────────────────────────────────────────────────────────────────────────────────┘
      │  file immagine, /dev/diskN (tramite authopen), oppure byte via HTTP/SFTP
```

- **I driver dei file system** sono scritti da zero in Rust: extent tree, directory htree, `metadata_csum` e replay del journal jbd2 per ext4; MFT, indici B-tree, reparse point e LZNT1 per NTFS; nomi lunghi VFAT per FAT; bitmap di allocazione e tabella up-case per exFAT.
- **Il server in loopback** ascolta solo su 127.0.0.1. Usa un percorso di export casuale, accetta un solo MOUNT e mette un segreto a 128 bit in ogni file handle, e rifiuta le chiamate di altri utenti.
- **L'app Swift** parla con il motore tramite una piccola interfaccia C (`core/include/lumendisk_ffi.h`) e usa DiskArbitration per scoprire i dischi.

## Verifica

`scripts/build_and_verify.sh` compila tutto e verifica il funzionamento contro strumenti ufficiali e indipendenti eseguiti in Docker (e2fsprogs, ntfs-3g, dosfstools, exfatprogs, OpenSSH, Apache). A ogni push e pull request la [CI](.github/workflows/ci.yml) lo esegue su un Mac di GitHub senza Docker e fuzzing: build universale, test unitari, race della cache, immagini corrotte, server LAN, avvio dell'app e schermate dell'interfaccia. Le suite end-to-end richiedono Docker, montaggi reali e dischi collegati, quindi girano in locale.

| Suite | Cosa dimostra |
|---|---|
| Unitari (136) | ogni driver supera la stessa suite di conformità; replay del journal dopo un crash; Ghost Shield; Shadow, compresi il replay del journal di sessioni casuali, journal troncati, un crash simulato a ogni passo dell'applicazione su ext4, FAT32, exFAT e NTFS (prima e subito dopo ogni passo, con un checkpoint troncato, e di nuovo durante le riprese) e modifiche fatte al disco altrove; un `fsync` della cartella che fallisce mentre una sessione viene eliminata (dopo un crash torna intera, mai a metà); group commit; race della cache remota; un panic di un driver mette fuori uso solo il suo volume, subito (EIO), senza avvelenare i lock |
| ext4 contro e2fsprogs (5) | volumi creati con il vero `mkfs.ext4` (64bit, flex_bg, metadata_csum, orphan_file) modificati da LumenDisk restano puliti per `e2fsck`; replay jbd2 di una transazione reale |
| End-to-end (37) | `mount_nfs` reale + client NFS del Finder su ext4 / FAT32 / exFAT / NTFS; Shadow, compreso il `kill -9` del server con modifiche in sospeso e a metà applicazione, poi `e2fsck`; nessuna finestra di perdita: `kill -9` nell'istante in cui un mkdir / create / rename / unlink / rmdir / close / fsync ritorna, in Shadow e in lettura/scrittura su ext4 e FAT32, senza perdere nulla di confermato e con `fsck` pulito; Shadow su un vero dispositivo a blocchi (`/dev/diskNs1`, `/dev/rdiskN`): `kill -9` senza attesa e a metà applicazione, ripresa, `e2fsck`; Shadow su SFTP, WebDAV e condivisioni LAN: `kill -9`, ripristino, applicazione verificata sul server, modifiche fatte sul server rilevate; riparazione; immagine Raspberry Pi; symlink NTFS; sicurezza del loopback; immagini via HTTP Range; condivisione LAN offline; SFTP; blackout con un file da 64 MB; WebDAV Apache con coda offline tra riavvii; TLS attraverso un proxy intercettatore (niente in chiaro); un dispositivo a blocchi reale collegato |
| Immagini corrotte (16.000 varianti + regressioni) | volumi validi e popolati (ext2/ext4 con extent, htree, symlink e una transazione del journal in sospeso; NTFS con blocchi di indice, liste di attributi, file compressi LZNT1 e reparse point; FAT12/16/32; exFAT) alterati privilegiando i metadati, più tabelle MBR/GPT alterate e un caso di regressione per ogni bug trovato: ogni variante viene montata in sola lettura e in lettura/scrittura, esplorata, modificata e riparata senza panic, senza blocchi (limite di 20 s) e senza allocazioni oltre 1 GiB. Un panic di un driver disattiva solo quel volume (da lì in poi EIO) e non lo blocca mai. `LUMENDISK_CORRUPT_CASES` aumenta il numero di casi |
| Server LAN (22) · SFTP (4) | protocollo WebDAV/portale su TLS; autenticazione, timeout e riconnessioni con OpenSSH |
| Interfaccia | ogni schermata guidata dal motore reale con volumi reali, in italiano e inglese |

## Sicurezza

Il modello delle minacce è descritto in [SECURITY.md](SECURITY.md). Copre:

- chi può raggiungere il server NFS locale e perché `AUTH_SYS` è sicuro solo dietro un filtro delle connessioni fail-closed;
- il portale Wi-Fi e la condivisione LAN;
- gli archivi remoti e i volumi non affidabili.

## Limiti noti

- I volumi compaiono in `~/Volumes/<Nome>`, perché un'app senza diritti di amministratore non può creare cartelle in `/Volumes`.
- I volumi remoti non possono aprire offline file mai aperti mentre c'era la rete; falliscono subito invece di restare bloccati.
- Sui telefoni, l'indirizzo HTTPS del portale Wi-Fi mostra una volta un avviso sul certificato. Per i telefoni che lo rifiutano si può attivare un indirizzo HTTP non cifrato, dopo un avviso esplicito. È spento per impostazione predefinita e, finché resta attivo, PIN e file viaggiano in chiaro sulla rete.
- NTFS: i file compressi e cifrati sono in sola lettura; non ancora verificato con `chkdsk` di Windows.
- I volumi ext4 con inline_data, cifratura, casefold, bigalloc, quota o verity vengono montati in sola lettura.
- La maggior parte degli adattatori USB nasconde i dati SMART a macOS; in quel caso LumenDisk lo dice chiaramente.
- Ogni close() o `fsync` di un file scritto attende un `F_FULLFSYNC` (circa 4 ms su un SSD Apple; le richieste concorrenti ne condividono uno), quindi copiare 1.500 file piccoli con `cp` richiede circa 9 s invece di circa 2 s. I file grandi ne risentono poco (150 MB: 0,23–0,32 s invece di 0,20–0,35 s). In caso di blackout possono mancare le modifiche più recenti di circa 100 ms non ancora chiuse o salvate con `fsync`. Sui volumi SFTP, WebDAV e LAN la garanzia è quella della cache remota, non questa.
- Shadow Mode: un'applicazione delle modifiche già iniziata non si può annullare; la garanzia è che si può sempre portare a termine. Il controllo che il disco non sia stato modificato altrove guarda i metadati del file system e i file toccati dalla sessione, quindi una modifica fatta altrove che non cambia né gli uni né gli altri può sfuggire; non viene fatto mentre si completa un'applicazione interrotta. Sui volumi SFTP, WebDAV e LAN il controllo confronta dimensione e data di modifica (al secondo su SFTP) di ciò che la sessione ha letto, più l'ETag su WebDAV, quindi una modifica sul server che li lascia tutti uguali può sfuggire. Le loro sessioni salvate si possono verificare e riprendere solo ricollegandosi al server, perché nessuna password viene salvata, e durante l'applicazione il Finder attende il caricamento.

## Supporto

Se LumenDisk ti è utile, puoi sostenerne lo sviluppo:

<a href="https://paypal.me/giuseppegambino92"><img src="https://www.paypalobjects.com/it_IT/IT/i/btn/btn_donateCC_LG.gif" alt="Fai una donazione con PayPal"></a>

---

© Giuseppe Gambino. Il server NFSv3 incluso in `vendor/nfsserve` è distribuito con licenza BSD-3-Clause (XetData / Hugging Face).
