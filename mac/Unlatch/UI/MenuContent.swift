import AppKit
import SwiftUI

struct MenuContent: View {
    @ObservedObject var model: AppModel

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            Text("Unlatch").font(.headline)
            agentBanner
            if model.snapshots.isEmpty {
                Text("No VMs yet. Add one to see its files in Finder.")
                    .font(.callout)
                    .foregroundStyle(.secondary)
            }
            ForEach(model.snapshots) { snapshot in
                DomainRow(snapshot: snapshot, model: model)
                Divider()
            }
            if let message = model.message {
                HStack(alignment: .top) {
                    Text(message).font(.caption).foregroundStyle(.red).textSelection(.enabled)
                    Spacer()
                    Button("Dismiss") { model.message = nil }.buttonStyle(.borderless).font(.caption)
                }
            }
            HStack {
                Button("Add VM…") { model.showAddVM() }
                Spacer()
                Button("Quit Unlatch") { NSApp.terminate(nil) }
            }
        }
        .padding(14)
        .frame(width: 360)
    }

    @ViewBuilder
    private var agentBanner: some View {
        switch model.agentState {
        case .requiresApproval:
            VStack(alignment: .leading, spacing: 4) {
                Text("Allow Unlatch to run in the background so Finder can reach your VMs.")
                    .font(.callout)
                Button("Open Login Items Settings") { model.openLoginItemsSettings() }
            }
        case let .failed(reason):
            VStack(alignment: .leading, spacing: 4) {
                Text(reason).font(.callout).foregroundStyle(.red)
                Button(model.repairing ? "Repairing…" : "Repair Background Agent") { Task { await model.repairAgent() } }
                    .disabled(model.repairing)
            }
        case .enabled, .unknown:
            EmptyView()
        }
    }
}

struct DomainRow: View {
    let snapshot: DomainSnapshot
    @ObservedObject var model: AppModel
    @State private var confirmingRemove = false

    init(snapshot: DomainSnapshot, model: AppModel) {
        self.snapshot = snapshot
        self.model = model
    }

    private var record: DomainRecord { snapshot.record }

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            HStack {
                Circle().fill(dotColor).frame(width: 8, height: 8)
                Text(record.displayName).font(.body.weight(.medium))
                Spacer()
                Text(rttText).font(.caption).foregroundStyle(.secondary).monospacedDigit()
            }
            Text("\(record.destination):\(record.remoteRoot)")
                .font(.caption)
                .foregroundStyle(.secondary)
                .lineLimit(1)
                .truncationMode(.middle)
            Text(stateText).font(.caption)
            if let error = snapshot.lastError {
                Text(error).font(.caption).foregroundStyle(.orange).textSelection(.enabled).lineLimit(4)
            }
            actions
        }
        .confirmationDialog("Remove \(record.displayName) from Finder?", isPresented: $confirmingRemove) {
            Button("Remove", role: .destructive) { model.remove(record.id) }
        } message: {
            Text("Files on the VM are not touched. Local downloads and Mac-only metadata are discarded. Edits that have not reached the VM yet (for example, saved while it was offline) are not uploaded: they are moved to a folder Unlatch shows you after removing.")
        }
    }

    @ViewBuilder
    private var actions: some View {
        if case let .paused(reason)? = snapshot.status?.state {
            // Mass-deletion guard (review §2(c)8): the engine stopped applying remote deletions.
            VStack(alignment: .leading, spacing: 4) {
                Text("Paused: \(reason)").font(.caption).foregroundStyle(.orange)
                HStack {
                    Button("Keep My Files") { model.confirmPaused(record.id, apply: false) }
                    Button("Apply Deletions", role: .destructive) { model.confirmPaused(record.id, apply: true) }
                }
            }
        }
        HStack {
            Button("Reveal in Finder") { model.revealInFinder(record) }
                .disabled(!record.registered)
            if needsAttention, !record.destination.isEmpty {
                Button("Connect…") { model.retry(record.id) }
            }
            Spacer()
            Button("Remove…") { confirmingRemove = true }
        }
        .buttonStyle(.borderless)
        .font(.caption)
    }

    private var needsAttention: Bool {
        switch snapshot.status?.state {
        case .needsUser?, .offline?, nil: return true
        default: return !record.registered
        }
    }

    private var dotColor: Color {
        switch snapshot.status?.state {
        case .live?: return .green
        case .syncing?, .connecting?: return .yellow
        case .paused?, .needsUser?: return .orange
        case .offline?, nil: return .red
        }
    }

    private var rttText: String {
        guard let us = snapshot.status?.rttUs else { return "" }
        return String(format: "%.0f ms", Double(us) / 1000)
    }

    private var stateText: String {
        guard let status = snapshot.status else { return "Engine not running" }
        let base: String
        switch status.state {
        case .connecting: base = "Connecting…"
        case let .syncing(received): base = "Syncing (\(received) items)…"
        case .live: base = "Live · \(status.entries) items"
        case let .offline(error, retry): base = "Offline: \(error) (retry in \(retry / 1000) s)"
        case let .needsUser(reason, url): base = "Needs you: \(reason)" + (url.map { " — \($0)" } ?? "")
        case .paused: base = "Paused"
        }
        let uploads = status.pendingUploads > 0 ? " · \(status.pendingUploads) uploading" : ""
        let warnings = status.server?.warnings.first.map { " · \($0)" } ?? ""
        return base + uploads + warnings
    }
}
