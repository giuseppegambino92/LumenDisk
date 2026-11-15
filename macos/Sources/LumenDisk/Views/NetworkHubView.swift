import SwiftUI

/// "Dischi di Rete & Remoti" overview: add a server or a remote image, nearby Macs, connections.
struct NetworkHubView: View {
    @Environment(AppModel.self) private var model

    var body: some View {
        DetailCanvas {
            PageHeader(title: L("Dischi di Rete & Remoti", "Network & Remote Disks"),
                       subtitle: L("Server, immagini su Internet e Mac vicini appaiono nel Finder come un disco. Se la rete cade continui a lavorare: nessun blocco del Finder.",
                                   "Servers, images on the web and nearby Macs show up in Finder as a disk. If the network drops you keep working: Finder never blocks."),
                       systemImage: "network", tint: .ldIndigo)
            HStack(alignment: .top, spacing: 14) {
                ActionTile(systemImage: "server.rack", tint: .ldSlate,
                           title: L("Connetti a Server…", "Connect to Server…"),
                           subtitle: L("SFTP o WebDAV, con password o chiave SSH.", "SFTP or WebDAV, with a password or an SSH key.")) {
                    model.activeSheet = .connectServer
                }
                ActionTile(systemImage: "arrow.down.circle.fill", tint: .ldPlum,
                           title: L("Monta Immagine da URL…", "Mount Image from URL…"),
                           subtitle: L("Apri un .img su Internet senza scaricarlo tutto.", "Open an .img on the web without downloading all of it.")) {
                    model.activeSheet = .imageURL
                }
            }
            nearbyCard
            if !model.remoteVolumes.isEmpty { connectedCard }
        }
        .navigationTitle(L("Rete e Server", "Network & Servers"))
    }

    private var nearbyCard: some View {
        VStack(alignment: .leading, spacing: 12) {
            HStack {
                CardHeader(title: L("Mac nelle vicinanze", "Nearby Macs"),
                           subtitle: L("Dischi condivisi da altri Mac con LumenDisk sulla stessa rete.",
                                       "Disks shared by other Macs running LumenDisk on the same network."))
                Spacer()
                if model.lanDiscovery.isBrowsing {
                    HStack(spacing: 6) {
                        ProgressView().controlSize(.small)
                        Text(L("Ricerca…", "Searching…")).font(.callout).foregroundStyle(.secondary)
                    }
                }
            }
            if model.lanDiscovery.peers.isEmpty {
                HStack(alignment: .top, spacing: 14) {
                    Image(systemName: "dot.radiowaves.left.and.right")
                        .font(.system(size: 26, weight: .light))
                        .symbolRenderingMode(.hierarchical)
                        .foregroundStyle(Color.teal)
                        .frame(width: 40)
                    VStack(alignment: .leading, spacing: 4) {
                        Text(L("Nessun Mac sta condividendo un disco", "No Mac is sharing a disk right now"))
                            .font(.body.weight(.medium))
                        Text(L("Sull'altro Mac apri LumenDisk, scegli un volume e attiva «Condividi nella Rete Locale». Apparirà qui da solo.",
                               "On the other Mac, open LumenDisk, pick a volume and turn on “Share on Local Network”. It will show up here by itself."))
                            .font(.callout)
                            .foregroundStyle(.secondary)
                            .fixedSize(horizontal: false, vertical: true)
                    }
                }
                .padding(14)
                .frame(maxWidth: .infinity, alignment: .leading)
                .background(Color.ldWell, in: RoundedRectangle(cornerRadius: 12, style: .continuous))
            } else {
                VStack(spacing: 0) {
                    ForEach(Array(model.lanDiscovery.peers.enumerated()), id: \.element.id) { index, peer in
                        if index > 0 { Divider().padding(.leading, 56) }
                        HStack(spacing: 12) {
                            IconBadge(systemImage: "desktopcomputer", tint: .teal, size: 32)
                            VStack(alignment: .leading, spacing: 2) {
                                HStack(spacing: 6) {
                                    Text(peer.name).font(.body.weight(.medium))
                                    if peer.filesystem != .unknown { FilesystemPill(kind: peer.filesystem, compact: true) }
                                }
                                Text(L("Richiede il codice di abbinamento", "Needs the pairing code"))
                                    .font(.caption)
                                    .foregroundStyle(.secondary)
                            }
                            Spacer()
                            Button(L("Connetti…", "Connect…")) { model.activeSheet = .pair(peer) }
                        }
                        .padding(.horizontal, 12)
                        .padding(.vertical, 10)
                    }
                }
                .background(Color.ldWell, in: RoundedRectangle(cornerRadius: 12, style: .continuous))
            }
        }
        .card()
    }

    private var connectedCard: some View {
        VStack(alignment: .leading, spacing: 12) {
            CardHeader(title: L("Collegati", "Connected"))
            VStack(spacing: 0) {
                ForEach(Array(model.remoteVolumes.enumerated()), id: \.element.id) { index, volume in
                    if index > 0 { Divider().padding(.leading, 56) }
                    Button {
                        model.selection = .volume(volume.id)
                    } label: {
                        HStack(spacing: 12) {
                            IconBadge(systemImage: volume.symbol, tint: volume.tint, size: 32)
                            VStack(alignment: .leading, spacing: 3) {
                                HStack(spacing: 6) {
                                    Text(volume.displayName).font(.body.weight(.medium))
                                    KindPill(kind: volume.kind, compact: true)
                                }
                                Text(volume.remoteHost ?? volume.kind.title)
                                    .font(.caption)
                                    .foregroundStyle(.secondary)
                                    .lineLimit(1)
                            }
                            Spacer()
                            HealthPill(health: NetworkHealth(volume: volume), compact: true)
                            Image(systemName: "chevron.right")
                                .font(.caption.weight(.semibold))
                                .foregroundStyle(.tertiary)
                        }
                        .padding(.horizontal, 12)
                        .padding(.vertical, 10)
                        .contentShape(Rectangle())
                    }
                    .buttonStyle(.plain)
                }
            }
            .background(Color.ldWell, in: RoundedRectangle(cornerRadius: 12, style: .continuous))
        }
        .card()
    }
}

/// Large clickable card used for primary actions on hub pages.
struct ActionTile: View {
    var systemImage: String
    var tint: Color
    var title: String
    var subtitle: String
    var action: () -> Void
    @State private var hovering = false

    var body: some View {
        Button(action: action) {
            HStack(alignment: .center, spacing: 14) {
                IconBadge(systemImage: systemImage, tint: tint, size: 42)
                VStack(alignment: .leading, spacing: 3) {
                    Text(title).font(.body.weight(.semibold)).foregroundStyle(.primary)
                    Text(subtitle)
                        .font(.callout)
                        .foregroundStyle(.secondary)
                        .fixedSize(horizontal: false, vertical: true)
                        .multilineTextAlignment(.leading)
                }
                Spacer(minLength: 0)
                Image(systemName: "chevron.right")
                    .font(.system(size: 12, weight: .semibold))
                    .foregroundStyle(.tertiary)
            }
            .padding(16)
            .frame(maxWidth: .infinity, alignment: .leading)
            .background(hovering ? Color.primary.opacity(0.03) : Color.clear)
            .background(Color.ldCard, in: RoundedRectangle(cornerRadius: 16, style: .continuous))
            .overlay(RoundedRectangle(cornerRadius: 16, style: .continuous).strokeBorder(hovering ? tint.opacity(0.35) : Color.ldBorder, lineWidth: 1))
            .contentShape(RoundedRectangle(cornerRadius: 16, style: .continuous))
        }
        .buttonStyle(.plain)
        .onHover { hovering = $0 }
        .animation(.snappy(duration: 0.18), value: hovering)
    }
}

/// Detail page of a Mac discovered over Bonjour.
struct PeerDetailView: View {
    @Environment(AppModel.self) private var model
    var peer: LANPeer

    var body: some View {
        DetailCanvas {
            HStack(spacing: 16) {
                IconBadge(systemImage: "desktopcomputer", tint: .teal, size: 60)
                VStack(alignment: .leading, spacing: 6) {
                    HStack(spacing: 8) {
                        Text(peer.name).font(.title2.weight(.semibold)).lineLimit(1)
                        if peer.filesystem != .unknown { FilesystemPill(kind: peer.filesystem) }
                    }
                    Label(L("Condiviso da un Mac nella tua rete locale", "Shared by a Mac on your local network"), systemImage: "dot.radiowaves.left.and.right")
                        .font(.callout)
                        .foregroundStyle(.secondary)
                }
                Spacer()
                StatusChip(title: L("Disponibile", "Available"), systemImage: "checkmark.circle.fill", tint: .ldGood)
            }
            .card()

            VStack(alignment: .leading, spacing: 14) {
                CardHeader(title: L("Come collegarsi", "How to connect"))
                StepRow(number: 1, text: L("Sul Mac che condivide, apri la scheda del volume in LumenDisk.",
                                           "On the sharing Mac, open the volume in LumenDisk."))
                StepRow(number: 2, text: L("Leggi il codice di abbinamento (es. ABCD-EFGH) sotto «Condividi nella Rete Locale».",
                                           "Read the pairing code (e.g. ABCD-EFGH) under “Share on Local Network”."))
                StepRow(number: 3, text: L("Premi «Connetti…» e inseriscilo: il disco apparirà nel Finder.",
                                           "Click “Connect…” and enter it: the disk will appear in Finder."))
            }
            .card()
        }
        .safeAreaInset(edge: .bottom, spacing: 0) {
            ActionBar {
                Spacer()
                Button {
                    model.activeSheet = .pair(peer)
                } label: {
                    Label(L("Connetti…", "Connect…"), systemImage: "link")
                }
                .buttonStyle(.borderedProminent)
            }
            .controlSize(.large)
        }
        .navigationTitle(peer.name)
    }
}

private struct StepRow: View {
    var number: Int
    var text: String

    var body: some View {
        HStack(alignment: .firstTextBaseline, spacing: 12) {
            Text("\(number)")
                .font(.system(.callout, design: .rounded).weight(.bold))
                .foregroundStyle(Color.teal)
                .frame(width: 24, height: 24)
                .background(Color.teal.opacity(0.14), in: Circle())
            Text(text).font(.callout).fixedSize(horizontal: false, vertical: true)
        }
    }
}
