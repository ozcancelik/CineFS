import AppKit
import Foundation
import SwiftUI
import UniformTypeIdentifiers

// MARK: - Logging (shared Rust logger: ~/Library/Logs/CineFS/cinefs.log)

func cineLog(_ level: String, _ message: String) {
    level.withCString { l in message.withCString { m in cinefs_log(l, m) } }
}

struct ResolveResultPayload: Codable {
    let status: String
    let count: Int
    let message: String
    var parApplied: Int? = nil
    var parTotal: Int? = nil
}

// MARK: - Design tokens

/// One accent colour (CineFS amber); colour is used only where it carries meaning.
enum Tone { case neutral, good, accent }

struct Pill: View {
    let text: String
    var tone: Tone = .neutral

    private var fg: Color {
        switch tone {
        case .neutral: return .secondary
        case .good: return Color(red: 0.30, green: 0.72, blue: 0.52)
        case .accent: return Color(red: 0.91, green: 0.66, blue: 0.20)
        }
    }

    var body: some View {
        Text(text)
            .font(.system(size: 10.5, weight: .medium))
            .foregroundStyle(fg)
            .padding(.horizontal, 7)
            .padding(.vertical, 2.5)
            .background(
                RoundedRectangle(cornerRadius: 5)
                    .fill(tone == .neutral ? Color.primary.opacity(0.06) : fg.opacity(0.12))
            )
            .overlay(
                RoundedRectangle(cornerRadius: 5)
                    .strokeBorder(tone == .neutral ? Color.primary.opacity(0.10) : fg.opacity(0.35), lineWidth: 0.5)
            )
    }
}

// MARK: - Data Models

struct MountedClipInfo: Codable, Identifiable {
    var id: String { folderName }
    let folderName: String
    let cameraModel: String
    let width: Int
    let height: Int
    let bitsPerPixel: Int
    let fps: Double
    let frameCount: Int
    let hasAudio: Bool
    let payloadMode: String
    let stretch: Double
    let stretchAxis: String
    let stretchBaked: Bool
    let stretchSource: String
    let blackLevel: Int
    let whiteLevel: Int
    let levelSource: String
    let focusPixels: Int
}

struct MountSummaryPayload: Codable {
    let sourcePath: String
    let sourcePaths: [String]
    let mountPoint: String
    let nfsPort: UInt16
    let totalBytes: UInt64
    let totalFiles: UInt64
    let indexTimeMs: Double
    let clips: [MountedClipInfo]
}

final class ActiveMountItem: Identifiable, ObservableObject {
    let id = UUID()
    let handle: OpaquePointer
    let summary: MountSummaryPayload
    let mountedAt = Date()

    @Published var cacheHits: UInt64 = 0
    @Published var cacheMisses: UInt64 = 0
    @Published var cachePrefetched: UInt64 = 0
    @Published var cacheBytes: Int = 0

    init(handle: OpaquePointer, summary: MountSummaryPayload) {
        self.handle = handle
        self.summary = summary
    }

    // Thumbnails: rendered on demand off the main thread. The lock + closed flag guarantee the C
    // handle is never used after unmount has started.
    private let thumbLock = NSLock()
    private var thumbClosed = false
    private var thumbCache: [Int: NSImage] = [:]

    func closeThumbnails() {
        thumbLock.lock()
        thumbClosed = true
        thumbLock.unlock()
    }

    private func makeImage(_ buf: UnsafeMutablePointer<UInt8>, _ w: UInt32, _ h: UInt32) -> NSImage? {
        let len = Int(w) * Int(h) * 4
        let data = Data(bytes: buf, count: len)
        cinefs_free_buffer(buf, len)
        guard let provider = CGDataProvider(data: data as CFData),
              let cg = CGImage(
                width: Int(w), height: Int(h), bitsPerComponent: 8, bitsPerPixel: 32,
                // Tagged sRGB, not Rec.709: the player's Metal layer shows the same Rec.709-encoded
                // values untagged (≈ sRGB), and a 709 tag would make ColorSync darken the preview.
                bytesPerRow: Int(w) * 4, space: CGColorSpace(name: CGColorSpace.sRGB)!,
                bitmapInfo: CGBitmapInfo(rawValue: CGImageAlphaInfo.noneSkipLast.rawValue),
                provider: provider, decode: nil, shouldInterpolate: true, intent: .defaultIntent)
        else { return nil }
        return NSImage(cgImage: cg, size: NSSize(width: Int(w), height: Int(h)))
    }

    /// Blocking (~50-150 ms first time): call from a background task.
    func thumbnail(index: Int) -> NSImage? {
        thumbLock.lock()
        defer { thumbLock.unlock() }
        if thumbClosed { return nil }
        if let img = thumbCache[index] { return img }
        var w: UInt32 = 0
        var h: UInt32 = 0
        guard let buf = cinefs_clip_thumbnail(handle, index, 320, &w, &h),
              let img = makeImage(buf, w, h) else { return nil }
        thumbCache[index] = img
        return img
    }

    // Hover scrubbing: frames are quantised to `scrubBuckets` positions per clip so they can be cached,
    // and only the newest request decodes (older ones that waited on the lock are dropped).
    static let scrubBuckets = 48

    /// Frame shown for scrub position `bucket` (clicking the preview starts playback there).
    static func scrubFrame(bucket: Int, frameCount: Int) -> Int {
        let n = max(1, min(frameCount, scrubBuckets))
        return n > 1 ? bucket * (frameCount - 1) / (n - 1) : 0
    }
    private let genLock = NSLock()
    private var latestScrub = 0
    private var scrubCache: [Int: NSImage] = [:]

    func nextScrubToken() -> Int {
        genLock.lock()
        defer { genLock.unlock() }
        latestScrub += 1
        return latestScrub
    }

    private func isLatestScrub(_ t: Int) -> Bool {
        genLock.lock()
        defer { genLock.unlock() }
        return t == latestScrub
    }

    func cachedScrub(clip: Int, bucket: Int) -> NSImage? {
        thumbLock.lock()
        defer { thumbLock.unlock() }
        return scrubCache[clip * 1000 + bucket]
    }

    /// Blocking: call from a background task. `nil` if superseded by a newer request or on failure.
    func scrubImage(clip: Int, bucket: Int, frameCount: Int, token: Int) -> NSImage? {
        thumbLock.lock()
        defer { thumbLock.unlock() }
        if thumbClosed { return nil }
        let key = clip * 1000 + bucket
        if let img = scrubCache[key] { return img }
        if !isLatestScrub(token) { return nil }
        let frame = ActiveMountItem.scrubFrame(bucket: bucket, frameCount: frameCount)
        var w: UInt32 = 0
        var h: UInt32 = 0
        guard let buf = cinefs_clip_frame_preview(handle, clip, frame, 320, &w, &h),
              let img = makeImage(buf, w, h) else { return nil }
        scrubCache[key] = img
        return img
    }

    func refreshStats() {
        var hits: UInt64 = 0
        var misses: UInt64 = 0
        var prefetched: UInt64 = 0
        var bytes: Int = 0
        cinefs_mount_cache_stats(handle, &hits, &misses, &prefetched, &bytes)
        if hits != cacheHits || misses != cacheMisses || prefetched != cachePrefetched || bytes != cacheBytes {
            cacheHits = hits
            cacheMisses = misses
            cachePrefetched = prefetched
            cacheBytes = bytes
        }
    }
}


// MARK: - GPU Player

/// NSView the wgpu surface renders into. Forwards size changes and key presses.
final class PlayerSurfaceView: NSView {
    var onResize: ((UInt32, UInt32) -> Void)?
    var onKey: ((NSEvent) -> Bool)?

    override var acceptsFirstResponder: Bool { true }
    override func viewDidMoveToWindow() {
        wantsLayer = true
        window?.makeFirstResponder(self)
    }
    override func layout() {
        super.layout()
        let px = convertToBacking(bounds)
        onResize?(UInt32(max(1, px.width)), UInt32(max(1, px.height)))
    }
    override func keyDown(with event: NSEvent) {
        if onKey?(event) != true { super.keyDown(with: event) }
    }
}

struct PlayerSurface: NSViewRepresentable {
    let controller: PlayerController
    func makeNSView(context: Context) -> PlayerSurfaceView {
        let v = PlayerSurfaceView()
        v.onResize = { [weak controller] w, h in controller?.resize(w, h) }
        v.onKey = { [weak controller] e in controller?.handleKey(e) ?? false }
        controller.attach(view: v)
        return v
    }
    func updateNSView(_ nsView: PlayerSurfaceView, context: Context) {}
}

@MainActor
final class PlayerController: ObservableObject {
    private weak var item: ActiveMountItem?
    private let clipIndex: Int
    private let startFrame: Int
    let title: String
    private var player: OpaquePointer?
    private var timer: Timer?
    private var statusTimer: Timer?
    private var pendingSize: (UInt32, UInt32) = (1, 1)

    @Published var position: Double = 0
    @Published var frameCount: Int = 1
    @Published var fps: Double = 24
    @Published var playing = false
    @Published var dropped: UInt64 = 0
    @Published var looping = true
    @Published var error: String?
    @Published var hasAudio = false

    private static let volumeKey = "com.ozcancelik.cinefs.playerVolume"
    private static let mutedKey = "com.ozcancelik.cinefs.playerMuted"

    /// Shared by every player window and remembered across launches.
    @Published var volume: Double = UserDefaults.standard.object(forKey: PlayerController.volumeKey) as? Double ?? 1.0 {
        didSet { UserDefaults.standard.set(volume, forKey: Self.volumeKey); applyVolume() }
    }
    @Published var muted: Bool = UserDefaults.standard.bool(forKey: PlayerController.mutedKey) {
        didSet { UserDefaults.standard.set(muted, forKey: Self.mutedKey); applyVolume() }
    }

    private func applyVolume() {
        if let p = player { cinefs_player_set_volume(p, muted ? 0 : Float(volume)) }
    }

    init(item: ActiveMountItem, clipIndex: Int, title: String, startFrame: Int = 0) {
        self.item = item
        self.clipIndex = clipIndex
        self.startFrame = startFrame
        self.title = title
    }

    func attach(view: NSView) {
        guard player == nil, let item else { return }
        var err: UnsafeMutablePointer<CChar>? = nil
        let (w, h) = pendingSize
        player = cinefs_player_create(item.handle, clipIndex, Unmanaged.passUnretained(view).toOpaque(), w, h, &err)
        if player == nil {
            if let err {
                error = String(cString: err)
                cinefs_free_string(err)
            } else {
                error = "Could not start the GPU player."
            }
            return
        }
        cinefs_player_set_loop(player, 1)
        if startFrame > 0 { cinefs_player_seek(player, Double(startFrame)) }
        hasAudio = cinefs_player_has_audio(player) != 0
        applyVolume()
        let t = Timer(timeInterval: 1.0 / 120.0, repeats: true) { [weak self] _ in
            MainActor.assumeIsolated {
                guard let self, let p = self.player else { return }
                _ = cinefs_player_tick(p)
            }
        }
        RunLoop.main.add(t, forMode: .common)
        timer = t
        let st = Timer(timeInterval: 1.0 / 15.0, repeats: true) { [weak self] _ in
            MainActor.assumeIsolated { self?.refreshState() }
        }
        RunLoop.main.add(st, forMode: .common)
        statusTimer = st
        refreshState()
    }

    func resize(_ w: UInt32, _ h: UInt32) {
        pendingSize = (w, h)
        if let p = player { cinefs_player_resize(p, w, h) }
    }

    private func refreshState() {
        guard let p = player else { return }
        var pos = 0.0, f = 24.0
        var count: UInt64 = 1, drops: UInt64 = 0
        var isPlaying: UInt32 = 0
        cinefs_player_state(p, &pos, &count, &f, &isPlaying, &drops)
        position = pos
        frameCount = Int(count)
        fps = f
        playing = isPlaying != 0
        dropped = drops
    }

    func toggle() { if let p = player { cinefs_player_toggle(p); refreshState() } }
    func seek(_ frame: Double) { if let p = player { cinefs_player_seek(p, frame); position = frame } }
    func step(_ d: Int) {
        guard let p = player else { return }
        cinefs_player_pause(p)
        refreshState()
        seek(min(max(Double(Int(position.rounded()) + d), 0), Double(frameCount - 1)))
    }
    func setLoop(_ on: Bool) { looping = on; if let p = player { cinefs_player_set_loop(p, on ? 1 : 0) } }

    func handleKey(_ e: NSEvent) -> Bool {
        switch e.keyCode {
        case 49: toggle()                      // space
        case 123: step(e.modifierFlags.contains(.shift) ? -10 : -1)   // left
        case 124: step(e.modifierFlags.contains(.shift) ? 10 : 1)     // right
        case 115: seek(0)                      // home
        case 119: seek(Double(frameCount - 1)) // end
        case 37: setLoop(!looping)             // L
        case 46: if hasAudio { muted.toggle() } // M
        default: return false
        }
        return true
    }

    func timecode(_ frame: Double) -> String {
        let f = max(0, Int(frame.rounded()))
        let r = max(1, Int(fps.rounded()))
        let s = f / r
        return String(format: "%02d:%02d:%02d:%02d", s / 3600, (s / 60) % 60, s % 60, f % r)
    }

    /// Stops the timers and frees the GPU player (safe to call twice).
    func shutdown() {
        timer?.invalidate(); timer = nil
        statusTimer?.invalidate(); statusTimer = nil
        if let p = player {
            player = nil
            cinefs_player_destroy(p)
        }
    }
}

struct PlayerScreen: View {
    @ObservedObject var c: PlayerController

    var body: some View {
        VStack(spacing: 0) {
            ZStack {
                Color.black
                PlayerSurface(controller: c)
                if let err = c.error {
                    Text(err).foregroundStyle(.red).padding()
                }
            }
            HStack(spacing: 12) {
                Button { c.toggle() } label: {
                    Image(systemName: c.playing ? "pause.fill" : "play.fill").frame(width: 18)
                }
                .keyboardShortcut(.space, modifiers: [])
                .buttonStyle(.borderless)

                Slider(
                    value: Binding(get: { c.position }, set: { c.seek($0) }),
                    in: 0...Double(max(1, c.frameCount - 1))
                )

                Text("\(c.timecode(c.position))  \(Int(c.position.rounded()) + 1)/\(c.frameCount)")
                    .font(.system(size: 12, design: .monospaced))
                    .foregroundStyle(.secondary)
                Text(String(format: "%.2f fps", c.fps))
                    .font(.system(size: 12, design: .monospaced))
                    .foregroundStyle(.secondary)
                if c.dropped > 0 {
                    Text("dropped \(c.dropped)")
                        .font(.system(size: 12, design: .monospaced))
                        .foregroundStyle(.orange)
                        .help("Frames skipped because they were not decoded in time")
                }
                if c.hasAudio {
                    Button { c.muted.toggle() } label: {
                        Image(systemName: c.muted || c.volume == 0 ? "speaker.slash.fill" : "speaker.wave.2.fill")
                            .frame(width: 18)
                    }
                    .buttonStyle(.borderless)
                    .help("Mute (M)")
                    Slider(value: $c.volume, in: 0...1)
                        .frame(width: 70)
                        .disabled(c.muted)
                        .help("Volume")
                }
                Toggle(isOn: Binding(get: { c.looping }, set: { c.setLoop($0) })) {
                    Image(systemName: "repeat")
                }
                .toggleStyle(.button)
                .help("Loop (L)")
            }
            .padding(.horizontal, 12)
            .padding(.vertical, 8)
            .background(.bar)
        }
        .frame(minWidth: 640, minHeight: 400)
    }
}

/// Owns the player windows so they can be closed when their mount goes away.
@MainActor
final class PlayerWindows: NSObject, NSWindowDelegate {
    static let shared = PlayerWindows()
    private var entries: [(window: NSWindow, controller: PlayerController, mountID: UUID)] = []

    func open(item: ActiveMountItem, clipIndex: Int, title: String, startFrame: Int = 0) {
        let controller = PlayerController(item: item, clipIndex: clipIndex, title: title, startFrame: startFrame)
        let win = NSWindow(
            contentRect: NSRect(x: 0, y: 0, width: 1280, height: 760),
            styleMask: [.titled, .closable, .resizable, .miniaturizable],
            backing: .buffered, defer: false)
        win.title = title
        win.isReleasedWhenClosed = false
        win.contentView = NSHostingView(rootView: PlayerScreen(c: controller))
        win.delegate = self
        win.center()
        entries.append((win, controller, item.id))
        win.makeKeyAndOrderFront(nil)
    }

    func windowWillClose(_ notification: Notification) {
        guard let w = notification.object as? NSWindow,
              let i = entries.firstIndex(where: { $0.window === w }) else { return }
        entries[i].controller.shutdown()
        entries.remove(at: i)
    }

    func closeAll(mountID: UUID) {
        for e in entries where e.mountID == mountID { e.window.close() }
    }

    func closeEverything() {
        for e in entries { e.window.close() }
    }
}

// MARK: - Mount Manager

/// A mount that is still being indexed: one row per clip file with its state (same as Windows).
/// `@unchecked Sendable`: the mounting thread only passes it around; its state is read and
/// changed exclusively on the main thread.
final class PendingMount: ObservableObject, Identifiable, @unchecked Sendable {
    enum ClipState: Equatable {
        case waiting, loading, ready
        case failed(String)
    }

    struct Clip: Identifiable {
        let id: Int
        let name: String
        var state: ClipState
    }

    let id = UUID()
    let title: String
    @Published var clips: [Clip] = []
    /// All clips indexed; the volume itself is being mounted.
    @Published var attaching = false

    init(title: String) {
        self.title = title
    }

    var failedNames: [String] {
        clips.compactMap { if case .failed = $0.state { return $0.name } else { return nil } }
    }

    /// Applies one progress event from `cinefs_mount_with_progress` (main thread).
    func apply(event: UInt32, index: Int, total: Int, text: String) {
        switch event {
        case 0:
            if index == clips.count {
                clips.append(Clip(id: index, name: URL(fileURLWithPath: text).lastPathComponent, state: .waiting))
            }
        case 1, 2, 3:
            guard clips.indices.contains(index) else { return }
            clips[index].state = event == 1 ? .loading : (event == 2 ? .ready : .failed(text))
            if event != 1 && index == total - 1 { attaching = true }
        default:
            break
        }
    }
}

/// C callback for `cinefs_mount_with_progress`: forwards each event to its `PendingMount` on the
/// main thread. `ctx` is an unretained `PendingMount` kept alive by the mounting closure.
private let mountProgressCallback: CineFsMountProgressFn = { ctx, event, index, total, text in
    guard let ctx else { return }
    let pending = Unmanaged<PendingMount>.fromOpaque(ctx).takeUnretainedValue()
    let message = text.map { String(cString: $0) } ?? ""
    let i = Int(index), n = Int(total)
    DispatchQueue.main.async { pending.apply(event: event, index: i, total: n, text: message) }
}

@MainActor
final class MountManager: ObservableObject {
    static let shared = MountManager()

    private static let resolvePathDefaultsKey = "com.ozcancelik.cinefs.resolveAppPath"

    @Published var mounts: [ActiveMountItem] = []
    /// Mounts still being indexed (shown above `mounts` with per-clip progress).
    @Published var pending: [PendingMount] = []
    /// UserDefaults keys for the persisted preferences (defaults for clips mounted from now on).
    private enum Pref {
        static let mode = "com.ozcancelik.cinefs.payloadMode"
        static let cacheMB = "com.ozcancelik.cinefs.cacheSizeMB"
        static let autoReveal = "com.ozcancelik.cinefs.autoRevealInFinder"
        static let stretchBake = "com.ozcancelik.cinefs.stretchBake"
        static let stretchOverride = "com.ozcancelik.cinefs.stretchOverride"
        static let focusFix = "com.ozcancelik.cinefs.focusFix"
    }

    @Published var selectedMode: UInt32 = 0 { // 0 = Auto, 1 = Passthrough, 2 = NativePacked, 3 = Unpacked16
        didSet { UserDefaults.standard.set(Int(selectedMode), forKey: Pref.mode) }
    }
    @Published var cacheSizeMB: Int = 512 {
        didSet { UserDefaults.standard.set(cacheSizeMB, forKey: Pref.cacheMB) }
    }
    /// Off by default: the CineFS window already lists, previews and sends the clips to Resolve.
    @Published var autoRevealInFinder: Bool = false {
        didSet { UserDefaults.standard.set(autoRevealInFinder, forKey: Pref.autoReveal) }
    }
    @Published var isMounting: Bool = false
    @Published var isImportingToResolve: Bool = false
    @Published var statusMessage: String? = nil
    @Published var errorMessage: String? = nil
    @Published var resolveAppPath: String? = nil

    /// How anamorphic crop_rec stretch is delivered (applies to clips mounted from now on).
    @Published var stretchBake: Bool = true {
        didSet {
            cinefs_set_stretch_bake(stretchBake ? 1 : 0)
            UserDefaults.standard.set(stretchBake, forKey: Pref.stretchBake)
        }
    }
    /// 0 = Auto (from file), 1 = Off, 2 = horizontal ×3, 3 = vertical ×1.67, 4 = vertical ×3
    @Published var stretchOverride: Int = 0 {
        didSet {
            Self.applyStretchOverride(stretchOverride)
            UserDefaults.standard.set(stretchOverride, forKey: Pref.stretchOverride)
        }
    }

    private static func applyStretchOverride(_ choice: Int) {
        let f: Float
        switch choice {
        case 1: f = 1.0
        case 2: f = 1.0 / 3.0
        case 3: f = 5.0 / 3.0
        case 4: f = 3.0
        default: f = 0
        }
        cinefs_set_stretch_override(f)
    }

    /// Focus-pixel (PDAF) correction for Canon 100D/650D/700D/EOS M clips mounted from now on.
    @Published var focusFix: Bool = true {
        didSet {
            cinefs_set_focus_fix(focusFix ? 1 : 0)
            UserDefaults.standard.set(focusFix, forKey: Pref.focusFix)
        }
    }

    private var statsTimer: Timer?

    private init() {
        "CineFS".withCString { cinefs_log_init($0) }

        // Restore the saved preferences (property observers do not run inside init, so the core
        // is configured explicitly below).
        let d = UserDefaults.standard
        if let v = d.object(forKey: Pref.mode) as? Int, (0...3).contains(v) { selectedMode = UInt32(v) }
        if let v = d.object(forKey: Pref.cacheMB) as? Int, v > 0 { cacheSizeMB = v }
        if let v = d.object(forKey: Pref.autoReveal) as? Bool { autoRevealInFinder = v }
        if let v = d.object(forKey: Pref.stretchBake) as? Bool { stretchBake = v }
        if let v = d.object(forKey: Pref.stretchOverride) as? Int, (0...4).contains(v) { stretchOverride = v }
        if let v = d.object(forKey: Pref.focusFix) as? Bool { focusFix = v }

        cinefs_set_stretch_bake(stretchBake ? 1 : 0)
        cinefs_set_focus_fix(focusFix ? 1 : 0)
        Self.applyStretchOverride(stretchOverride)
        resolveAppPath = detectResolveAppPath()
        statsTimer = Timer.scheduledTimer(withTimeInterval: 1.0, repeats: true) { [weak self] _ in
            Task { @MainActor in
                self?.refreshAllStats()
            }
        }
    }

    var mountRootDirectory: URL {
        let home = FileManager.default.homeDirectoryForCurrentUser
        return home.appendingPathComponent("CineFS", isDirectory: true)
    }

    func refreshAllStats() {
        for item in mounts {
            item.refreshStats()
        }
    }

    // MARK: - DaVinci Resolve Discovery & Selection

    func detectResolveAppPath() -> String? {
        let fm = FileManager.default
        if let saved = UserDefaults.standard.string(forKey: Self.resolvePathDefaultsKey),
           fm.fileExists(atPath: saved) {
            return saved
        }
        if let bundleURL = NSWorkspace.shared.urlForApplication(withBundleIdentifier: "com.blackmagic-design.DaVinciResolve"),
           fm.fileExists(atPath: bundleURL.path) {
            UserDefaults.standard.set(bundleURL.path, forKey: Self.resolvePathDefaultsKey)
            return bundleURL.path
        }
        let standardCandidates = [
            "/Applications/DaVinci Resolve/DaVinci Resolve.app",
            "/Applications/DaVinci Resolve.app"
        ]
        for candidate in standardCandidates where fm.fileExists(atPath: candidate) {
            UserDefaults.standard.set(candidate, forKey: Self.resolvePathDefaultsKey)
            return candidate
        }
        return nil
    }

    @discardableResult
    func promptChooseResolveApp() -> URL? {
        let panel = NSOpenPanel()
        panel.title = "Locate DaVinci Resolve.app"
        panel.message = "Select your DaVinci Resolve application bundle so CineFS can import clips directly into your Media Pool."
        panel.canChooseFiles = true
        panel.canChooseDirectories = false
        panel.allowsMultipleSelection = false
        panel.allowedContentTypes = [.application]
        panel.directoryURL = URL(fileURLWithPath: "/Applications")
        NSApp.activate(ignoringOtherApps: true)
        if panel.runModal() == .OK, let url = panel.url {
            UserDefaults.standard.set(url.path, forKey: Self.resolvePathDefaultsKey)
            resolveAppPath = url.path
            statusMessage = "DaVinci Resolve path set to \(url.lastPathComponent)"
            return url
        }
        return nil
    }

    func ensureResolveAppURL() -> URL? {
        if let existing = detectResolveAppPath() {
            resolveAppPath = existing
            return URL(fileURLWithPath: existing)
        }
        return promptChooseResolveApp()
    }

    // MARK: - Mount & Unmount

    func mountSource(url: URL) {
        mountSources(urls: [url])
    }

    /// Mounts one OR MANY files/folders into a SINGLE virtual volume.
    func mountSources(urls: [URL]) {
        guard !urls.isEmpty else { return }
        let sourcePaths = urls.map { $0.path }
        if urls.count == 1,
           let existing = mounts.first(where: { $0.summary.sourcePaths.contains(sourcePaths[0]) }) {
            revealInFinder(path: existing.summary.mountPoint)
            return
        }

        let baseName = urls.count == 1
            ? urls[0].deletingPathExtension().lastPathComponent
            : "\(urls[0].deletingPathExtension().lastPathComponent)_+\(urls.count - 1)"
        let rootDir = mountRootDirectory
        try? FileManager.default.createDirectory(at: rootDir, withIntermediateDirectories: true)

        var targetMountURL = rootDir.appendingPathComponent(baseName, isDirectory: true)
        var suffix = 2
        while mounts.contains(where: { $0.summary.mountPoint == targetMountURL.path }) {
            targetMountURL = rootDir.appendingPathComponent("\(baseName)_\(suffix)", isDirectory: true)
            suffix += 1
        }

        let mountPointPath = targetMountURL.path
        let mode = selectedMode
        let cacheMB = cacheSizeMB
        let shouldReveal = autoRevealInFinder

        isMounting = true
        errorMessage = nil

        let pendingItem = PendingMount(
            title: urls.count == 1 ? urls[0].lastPathComponent : "\(urls.count) items"
        )
        pending.insert(pendingItem, at: 0)

        DispatchQueue.global(qos: .userInitiated).async {
            var errPtr: UnsafeMutablePointer<CChar>? = nil
            let progressCtx = Unmanaged.passUnretained(pendingItem).toOpaque()
            let rawHandle = sourcePaths.joined(separator: "\n").withCString { srcCStr in
                mountPointPath.withCString { mpCStr in
                    cinefs_mount_with_progress(srcCStr, mpCStr, mode, cacheMB, &errPtr, mountProgressCallback, progressCtx)
                }
            }
            // Runs after the progress events already queued on the main thread.
            DispatchQueue.main.async {
                self.pending.removeAll { $0.id == pendingItem.id }
            }

            guard let handle = rawHandle else {
                var errMsg = "Failed to mount \(urls[0].lastPathComponent)"
                if let errPtr = errPtr {
                    errMsg = String(cString: errPtr)
                    cinefs_free_string(errPtr)
                }
                cineLog("ERROR", "mount failed: \(errMsg)")
                DispatchQueue.main.async {
                    self.isMounting = false
                    self.errorMessage = errMsg
                }
                return
            }

            var parsedSummary: MountSummaryPayload? = nil
            if let jsonCStr = cinefs_mount_summary_json(handle) {
                let jsonString = String(cString: jsonCStr)
                if let data = jsonString.data(using: .utf8) {
                    parsedSummary = try? JSONDecoder().decode(MountSummaryPayload.self, from: data)
                }
            }

            guard let summary = parsedSummary else {
                cinefs_unmount(handle)
                DispatchQueue.main.async {
                    self.isMounting = false
                    self.errorMessage = "Failed to parse mount summary JSON."
                }
                return
            }

            DispatchQueue.main.async {
                let item = ActiveMountItem(handle: handle, summary: summary)
                self.mounts.insert(item, at: 0)
                self.isMounting = false
                let failed = pendingItem.failedNames
                self.statusMessage = "Mounted \(summary.clips.count) clip(s) at \(summary.mountPoint)"
                    + (failed.isEmpty ? "" : " — \(failed.count) file(s) could not be opened: \(failed.joined(separator: ", "))")
                cineLog("INFO", "UI: mounted \(summary.clips.count) clip(s) at \(summary.mountPoint)")
                if shouldReveal {
                    self.revealInFinder(path: summary.mountPoint)
                }
            }
        }
    }

    func unmount(item: ActiveMountItem) {
        guard let idx = mounts.firstIndex(where: { $0.id == item.id }) else { return }
        PlayerWindows.shared.closeAll(mountID: item.id)
        item.closeThumbnails()
        mounts.remove(at: idx)
        let handleAddr = Int(bitPattern: item.handle)
        let mp = item.summary.mountPoint
        DispatchQueue.global(qos: .userInitiated).async {
            if let ptr = OpaquePointer(bitPattern: handleAddr) {
                cinefs_unmount(ptr)
            }
            try? FileManager.default.removeItem(atPath: mp)
        }
    }

    func unmountAllSync() {
        let active = mounts
        mounts.removeAll()
        PlayerWindows.shared.closeEverything()
        for item in active {
            item.closeThumbnails()
            cinefs_unmount(item.handle)
            try? FileManager.default.removeItem(atPath: item.summary.mountPoint)
        }
    }

    func revealInFinder(path: String) {
        NSWorkspace.shared.selectFile(nil, inFileViewerRootedAtPath: path)
    }

    /// Sends all clips in `item` (or a single clip folder) directly into DaVinci Resolve's Media Pool via `fuscript`.
    func sendToResolve(item: ActiveMountItem, specificClipFolder: String? = nil) {
        guard let resolveURL = ensureResolveAppURL() else {
            return
        }

        let clipPaths: [String]
        if let folder = specificClipFolder {
            clipPaths = [(item.summary.mountPoint as NSString).appendingPathComponent(folder)]
        } else {
            clipPaths = item.summary.clips.map {
                (item.summary.mountPoint as NSString).appendingPathComponent($0.folderName)
            }
        }

        if clipPaths.isEmpty {
            return
        }

        isImportingToResolve = true
        errorMessage = nil
        statusMessage = "Importing \(clipPaths.count) clip(s) into DaVinci Resolve Media Pool..."

        // Bring DaVinci Resolve to front (or launch it if not running)
        let config = NSWorkspace.OpenConfiguration()
        config.activates = true
        NSWorkspace.shared.openApplication(at: resolveURL, configuration: config) { _, _ in }

        let fuscriptURL = resolveURL.appendingPathComponent("Contents/Libraries/Fusion/fuscript")
        // Pixel aspect ratio (pixel width / height) for clips that need an anamorphic stretch
        let parSpec = item.summary.clips
            .filter { abs($0.stretch - 1.0) > 0.01 && !$0.stretchBaked }
            .map { "\($0.folderName)\t\($0.stretchAxis == "horizontal" ? $0.stretch : 1.0 / $0.stretch)" }
            .joined(separator: "\n")
        let mountPoint = item.summary.mountPoint

        DispatchQueue.global(qos: .userInitiated).async {
            guard FileManager.default.isExecutableFile(atPath: fuscriptURL.path) else {
                DispatchQueue.main.async {
                    self.isImportingToResolve = false
                    self.revealInFinder(path: mountPoint)
                    self.statusMessage = "Opened DaVinci Resolve and revealed folder in Finder."
                }
                return
            }

            let outcome: ResolveResultPayload = clipPaths.joined(separator: "\n").withCString { clipsC in
                parSpec.withCString { parC in
                fuscriptURL.path.withCString { fuscriptC in
                    guard let raw = cinefs_resolve_import(fuscriptC, clipsC, parC, 45) else {
                        return ResolveResultPayload(status: "failed", count: 0, message: "no response from core")
                    }
                    defer { cinefs_free_string(raw) }
                    let data = Data(String(cString: raw).utf8)
                    return (try? JSONDecoder().decode(ResolveResultPayload.self, from: data))
                        ?? ResolveResultPayload(status: "failed", count: 0, message: "unreadable core response")
                }
                }
            }

            DispatchQueue.main.async {
                self.isImportingToResolve = false
                switch outcome.status {
                case "imported":
                    if let total = outcome.parTotal, total > 0 {
                        let applied = outcome.parApplied ?? 0
                        if applied == total {
                            self.statusMessage = "✓ Imported \(outcome.count) clip(s); pixel aspect ratio set on \(applied) stretched clip(s)"
                        } else {
                            self.errorMessage = "Imported \(outcome.count) clip(s), but Resolve refused the pixel aspect change on \(total - applied) of \(total) stretched clip(s). See ~/Library/Logs/CineFS/cinefs.log"
                        }
                    } else {
                        self.statusMessage = "✓ Imported \(outcome.count) CinemaDNG clip(s) into DaVinci Resolve Media Pool"
                    }
                case "imported_nothing":
                    self.revealInFinder(path: mountPoint)
                    self.errorMessage = "Resolve imported 0 items (it could not read the virtual folder, or the clips are already in the Media Pool). Revealed in Finder."
                case "no_project":
                    self.revealInFinder(path: mountPoint)
                    self.errorMessage = "Please open a project in DaVinci Resolve first, then click Resolve again (or drag from Finder)."
                case "no_resolve":
                    self.revealInFinder(path: mountPoint)
                    self.errorMessage = "Cannot reach DaVinci Resolve. Start it and enable Preferences > System > General > External scripting using: Local (Studio only). Revealed in Finder."
                default:
                    self.revealInFinder(path: mountPoint)
                    self.errorMessage = "Resolve import failed: \(outcome.message). Revealed in Finder."
                }
                cineLog("INFO", "UI: Resolve import -> \(outcome.status) (\(outcome.count)) \(outcome.message)")
            }
        }
    }

    func promptAddDialog() {
        let panel = NSOpenPanel()
        panel.title = "Select .MLV / .mcraw / .braw Clip(s) or Folder"
        panel.canChooseFiles = true
        panel.canChooseDirectories = true
        panel.allowsMultipleSelection = true
        panel.allowedContentTypes = [.folder, .item]
        NSApp.activate(ignoringOtherApps: true)
        if panel.runModal() == .OK {
            mountSources(urls: panel.urls)
        }
    }
}

// MARK: - App Delegate

final class AppDelegate: NSObject, NSApplicationDelegate {
    func application(_ application: NSApplication, open urls: [URL]) {
        Task { @MainActor in
            MountManager.shared.mountSources(urls: urls)
        }
    }

    func applicationWillTerminate(_ notification: Notification) {
        MainActor.assumeIsolated {
            MountManager.shared.unmountAllSync()
        }
    }
}

// MARK: - Views

struct MainMountWindowView: View {
    @ObservedObject var manager = MountManager.shared
    @State private var isDropTargeted = false

    var body: some View {
        VStack(spacing: 0) {
            // Header Bar
            HStack(spacing: 12) {
                Image(nsImage: NSApp.applicationIconImage)
                    .resizable()
                    .interpolation(.high)
                    .frame(width: 32, height: 32)

                VStack(alignment: .leading, spacing: 2) {
                    Text("CineFS")
                        .font(.system(size: 17, weight: .semibold))
                    Text("Virtual CinemaDNG for .MLV, .mcraw and .braw")
                        .font(.system(size: 11))
                        .foregroundStyle(.secondary)
                }

                Spacer()

                Picker("Mode", selection: $manager.selectedMode) {
                    Text("Auto (Fastest)").tag(UInt32(0))
                    Text("LJ92 Passthrough").tag(UInt32(1))
                    Text("Native Bit-Depth").tag(UInt32(2))
                    Text("Unpacked 16-bit").tag(UInt32(3))
                }
                .pickerStyle(.menu)
                .frame(width: 195)

                Button {
                    manager.promptAddDialog()
                } label: {
                    Label("Add Clip / Folder...", systemImage: "plus.circle.fill")
                }
                .buttonStyle(.borderedProminent)
                .tint(.orange)
            }
            .padding(16)
            .background(Color(nsColor: .windowBackgroundColor))

            Divider()

            if let err = manager.errorMessage {
                HStack {
                    Image(systemName: "exclamationmark.triangle.fill")
                        .foregroundStyle(.red)
                    Text(err)
                        .font(.system(size: 12))
                    Spacer()
                    Button("Dismiss") {
                        manager.errorMessage = nil
                    }
                    .buttonStyle(.borderless)
                }
                .padding(.horizontal, 16)
                .padding(.vertical, 8)
                .background(Color.red.opacity(0.10))
            } else if let status = manager.statusMessage {
                HStack {
                    if manager.isImportingToResolve {
                        ProgressView()
                            .controlSize(.small)
                    } else {
                        Image(systemName: "checkmark.circle")
                            .foregroundStyle(.secondary)
                    }
                    Text(status)
                        .font(.system(size: 12))
                    Spacer()
                    Button("Dismiss") {
                        manager.statusMessage = nil
                    }
                    .buttonStyle(.borderless)
                }
                .padding(.horizontal, 16)
                .padding(.vertical, 6)
                .background(Color.primary.opacity(0.04))
            }

            // Drop Zone + Mount List
            ZStack {
                if manager.mounts.isEmpty && manager.pending.isEmpty {
                    VStack(spacing: 14) {
                        Image(systemName: "arrow.down.doc.fill")
                            .font(.system(size: 42))
                            .foregroundStyle(isDropTargeted ? .orange : .secondary)

                        Text("Drop .MLV / .mcraw / .braw Clips or Folders Here")
                            .font(.system(size: 16, weight: .semibold))

                        Text("Instantly mounts virtual CinemaDNG folders + Broadcast WAV under ~/CineFS\nwith zero disk duplication, zero-decode LJ92 passthrough, and no kernel extensions.")
                            .font(.system(size: 12))
                            .foregroundStyle(.secondary)
                            .multilineTextAlignment(.center)
                            .frame(maxWidth: 460)

                        Button("Browse Files or Folders...") {
                            manager.promptAddDialog()
                        }
                        .buttonStyle(.bordered)
                        .padding(.top, 4)
                    }
                    .frame(maxWidth: .infinity, maxHeight: .infinity)
                    .padding(24)
                } else {
                    ScrollView {
                        LazyVStack(spacing: 12) {
                            ForEach(manager.pending) { p in
                                PendingMountCardView(pending: p)
                            }
                            ForEach(manager.mounts) { item in
                                MountCardView(item: item)
                            }
                        }
                        .padding(16)
                    }
                }
            }
            .background(
                RoundedRectangle(cornerRadius: 10)
                    .strokeBorder(
                        isDropTargeted ? Color.orange : Color.clear,
                        style: StrokeStyle(lineWidth: 2, dash: [6])
                    )
                    .padding(8)
            )
            .onDrop(of: [.fileURL], isTargeted: $isDropTargeted) { providers in
                handleDrop(providers: providers)
            }

            Divider()

            // Footer Bar
            HStack(spacing: 12) {
                Toggle("Open in Finder on mount", isOn: $manager.autoRevealInFinder)
                    .toggleStyle(.checkbox)
                    .font(.system(size: 12))

                Picker("Stretch", selection: $manager.stretchBake) {
                    Text("Baked into DNG").tag(true)
                    Text("Metadata only").tag(false)
                }
                .pickerStyle(.menu)
                .controlSize(.small)
                .frame(width: 170)
                .help("Anamorphic crop_rec clips: bake the stretch into the DNG pixels (works in every app, ~3× data for 3x1) or only write the DefaultScale tag (fast). Applies to the next mount.")

                Picker("Focus pixels", selection: $manager.focusFix) {
                    Text("Fix").tag(true)
                    Text("Off").tag(false)
                }
                .pickerStyle(.menu)
                .controlSize(.small)
                .frame(width: 140)
                .help("Canon 100D / 650D / 700D / EOS M: remove the bright PDAF dots. The dot pattern is learned from the clip; pixels are rewritten, so LJ92 passthrough is not used for those clips. Applies to the next mount.")

                Picker("Override", selection: $manager.stretchOverride) {
                    Text("Auto").tag(0)
                    Text("Off").tag(1)
                    Text("Horizontal ×3").tag(2)
                    Text("Vertical ×1.67").tag(3)
                    Text("Vertical ×3").tag(4)
                }
                .pickerStyle(.menu)
                .controlSize(.small)
                .frame(width: 170)
                .help("Force the stretch if the file's RAWC block is missing or wrong. Applies to the next mount.")

                Button {
                    manager.promptChooseResolveApp()
                } label: {
                    HStack(spacing: 4) {
                        Image(systemName: "gearshape")
                        Text(manager.resolveAppPath != nil ? "Resolve: Found" : "Locate Resolve.app...")
                    }
                    .font(.system(size: 11))
                }
                .buttonStyle(.bordered)
                .controlSize(.small)
                .help(manager.resolveAppPath ?? "Select DaVinci Resolve.app location")

                Spacer()

                if manager.isMounting {
                    ProgressView()
                        .controlSize(.small)
                    Text("Indexing & mounting...")
                        .font(.system(size: 12))
                        .foregroundStyle(.secondary)
                } else {
                    Text("\(manager.mounts.count) active mount\(manager.mounts.count == 1 ? "" : "s") • ~/CineFS")
                        .font(.system(size: 11))
                        .foregroundStyle(.secondary)
                }

                if !manager.mounts.isEmpty {
                    Button("Unmount All") {
                        manager.unmountAllSync()
                    }
                    .buttonStyle(.bordered)
                    .controlSize(.small)
                }
            }
            .padding(.horizontal, 16)
            .padding(.vertical, 10)
            .background(Color(nsColor: .windowBackgroundColor))
        }
        .frame(minWidth: 700, minHeight: 460)
    }

    private func handleDrop(providers: [NSItemProvider]) -> Bool {
        let fileProviders = providers.filter { $0.hasItemConformingToTypeIdentifier(UTType.fileURL.identifier) }
        guard !fileProviders.isEmpty else { return false }
        let group = DispatchGroup()
        let lock = NSLock()
        var urls: [URL] = []
        for provider in fileProviders {
            group.enter()
            provider.loadItem(forTypeIdentifier: UTType.fileURL.identifier, options: nil) { item, _ in
                if let data = item as? Data,
                   let url = URL(dataRepresentation: data, relativeTo: nil) {
                    lock.lock(); urls.append(url); lock.unlock()
                }
                group.leave()
            }
        }
        group.notify(queue: .main) {
            // One volume for the whole drop (stable order).
            let sorted = urls.sorted { $0.path < $1.path }
            Task { @MainActor in manager.mountSources(urls: sorted) }
        }
        return true
    }
}

/// Aligned clip table: one row per clip, fixed numeric columns, flexible (middle-truncated) name column.
/// Preview slot: shows the clip's middle frame; moving the mouse across it scrubs through the clip.
struct ClipThumbView: View {
    let item: ActiveMountItem
    let index: Int
    let frameCount: Int
    let title: String
    @State private var image: NSImage?
    @State private var failed = false
    @State private var scrubImage: NSImage?
    @State private var hoverFrac: CGFloat? = nil
    @State private var lastBucket = -1

    private let slotW: CGFloat = 96
    private let slotH: CGFloat = 54

    var body: some View {
        ZStack(alignment: .bottomLeading) {
            RoundedRectangle(cornerRadius: 3).fill(Color.black.opacity(0.35))
            if let shown = scrubImage ?? image {
                Image(nsImage: shown)
                    .resizable()
                    .aspectRatio(contentMode: .fit)
                    .frame(width: slotW, height: slotH)
            } else if failed {
                Image(systemName: "film").foregroundStyle(.tertiary).frame(width: slotW, height: slotH)
            } else {
                ProgressView().controlSize(.mini).frame(width: slotW, height: slotH)
            }
            if hoverFrac != nil {
                // Small translucent play badge: says "click to play" without hiding the frame.
                Image(systemName: "play.fill")
                    .font(.system(size: 9, weight: .bold))
                    .foregroundStyle(.white.opacity(0.92))
                    .offset(x: 1)
                    .frame(width: 22, height: 22)
                    .background(Circle().fill(Color.black.opacity(0.5)))
                    .frame(width: slotW, height: slotH)
                    .allowsHitTesting(false)
            }
            if let f = hoverFrac {
                Rectangle()
                    .fill(Color.accentColor)
                    .frame(width: max(2, slotW * f), height: 2)
            }
        }
        .frame(width: slotW, height: slotH)
        .clipShape(RoundedRectangle(cornerRadius: 3))
        .contentShape(Rectangle())
        .onTapGesture {
            // Play from the frame the preview is scrubbed to (from the start when not scrubbed).
            let start = lastBucket >= 0 ? ActiveMountItem.scrubFrame(bucket: lastBucket, frameCount: frameCount) : 0
            PlayerWindows.shared.open(item: item, clipIndex: index, title: title, startFrame: start)
        }
        .onContinuousHover { phase in
            switch phase {
            case .active(let p):
                let f = min(max(p.x / slotW, 0), 1)
                hoverFrac = f
                scrub(to: f)
            case .ended:
                hoverFrac = nil
                lastBucket = -1
                _ = item.nextScrubToken() // drop pending requests
                scrubImage = nil
            }
        }
        .task(id: index) {
            if image != nil { return }
            let it = item
            let i = index
            let img = await Task.detached(priority: .utility) { it.thumbnail(index: i) }.value
            if let img { image = img } else { failed = true }
        }
        .help("Preview — move the mouse across it to scrub, click to play from there (Rec.709, as-shot white balance; not a graded render)")
    }

    private func scrub(to f: CGFloat) {
        let n = max(1, min(frameCount, ActiveMountItem.scrubBuckets))
        let bucket = min(n - 1, Int(f * CGFloat(n)))
        if bucket == lastBucket { return }
        lastBucket = bucket
        if let cached = item.cachedScrub(clip: index, bucket: bucket) {
            scrubImage = cached
            return
        }
        let token = item.nextScrubToken()
        let it = item, i = index, fc = frameCount
        Task.detached(priority: .userInitiated) {
            let img = it.scrubImage(clip: i, bucket: bucket, frameCount: fc, token: token)
            await MainActor.run {
                if let img, lastBucket == bucket { scrubImage = img }
            }
        }
    }
}

struct ClipTableView: View {
    @ObservedObject var item: ActiveMountItem
    @ObservedObject var manager = MountManager.shared

    private func clipPath(_ folder: String) -> String {
        (item.summary.mountPoint as NSString).appendingPathComponent(folder)
    }

    private func stretchText(_ v: Double) -> String {
        v == v.rounded() ? "×\(Int(v))" : String(format: "×%.2f", v)
    }

    private func header(_ text: String, _ alignment: Alignment = .trailing) -> some View {
        Text(text.uppercased())
            .font(.system(size: 10, weight: .semibold))
            .foregroundStyle(.tertiary)
            .frame(maxWidth: .infinity, alignment: alignment)
    }

    var body: some View {
        Grid(alignment: .center, horizontalSpacing: 14, verticalSpacing: 0) {
            GridRow {
                Color.clear.frame(width: 96, height: 1)
                header("Clip", .leading).gridColumnAlignment(.leading)
                header("Resolution").frame(width: 84)
                header("Bit").frame(width: 34)
                header("FPS").frame(width: 46)
                header("Frames").frame(width: 52)
                header("Stretch").frame(width: 56)
                header("Audio", .center).frame(width: 40)
                header("Mode", .leading).frame(width: 118)
                Color.clear.frame(width: 24, height: 1)
            }
            .padding(.bottom, 6)

            Divider().gridCellColumns(10)

            ForEach(Array(item.summary.clips.enumerated()), id: \.element.id) { idx, clip in
                GridRow {
                    ClipThumbView(item: item, index: idx, frameCount: clip.frameCount, title: clip.folderName)

                    // Clicking the name opens the clip folder (same as Windows).
                    Button {
                        manager.revealInFinder(path: clipPath(clip.folderName))
                    } label: {
                        Text(clip.folderName)
                            .font(.system(size: 12, design: .monospaced))
                            .lineLimit(1)
                            .truncationMode(.middle)
                            .frame(minWidth: 140, maxWidth: .infinity, alignment: .leading)
                            .contentShape(Rectangle())
                    }
                    .buttonStyle(.plain)
                    .help("\(clip.folderName) — click to show the clip folder in Finder")

                    Text("\(clip.width)×\(clip.height)")
                        .frame(width: 84, alignment: .trailing)
                    Text("\(clip.bitsPerPixel)")
                        .frame(width: 34, alignment: .trailing)
                        .help("Black level \(clip.blackLevel) · White level \(clip.whiteLevel) — " + (clip.levelSource == "fallback" ? "file values were invalid, corrected" : "from the MLV file"))
                    Text(String(format: "%.2f", clip.fps))
                        .frame(width: 46, alignment: .trailing)
                    Text("\(clip.frameCount)")
                        .frame(width: 52, alignment: .trailing)

                    Group {
                        if abs(clip.stretch - 1.0) > 0.01 {
                            Pill(text: stretchText(clip.stretch), tone: .accent)
                                .help("Anamorphic crop_rec (\(clip.stretchSource)): \(clip.stretchAxis) stretch \(stretchText(clip.stretch)) — " + (clip.stretchBaked ? "baked into the DNG pixels (square pixels in any app)" : "written to the DNG DefaultScale tag only (apps may ignore it)"))
                        } else {
                            Text("–").foregroundStyle(.tertiary)
                        }
                    }
                    .frame(width: 56, alignment: .trailing)

                    Group {
                        if clip.hasAudio {
                            Image(systemName: "waveform").foregroundStyle(.secondary)
                        } else {
                            Text("–").foregroundStyle(.tertiary)
                        }
                    }
                    .frame(width: 40, alignment: .center)

                    Pill(
                        text: clip.focusPixels > 0
                            ? (clip.payloadMode.contains("Native") ? "Native + FP" : "16-bit + FP")
                            : clip.payloadMode.contains("Passthrough") ? "LJ92 Passthrough" :
                              clip.payloadMode.contains("Native") ? "Native Packed" : "16-bit Unpacked",
                        tone: clip.payloadMode.contains("Passthrough") ? .good : .neutral
                    )
                    .frame(width: 118, alignment: .leading)
                    .help(clip.focusPixels > 0
                        ? "Focus pixels (PDAF dots) corrected: \(clip.focusPixels) pixels per frame, learned from the clip. Pixels are rewritten, so LJ92 passthrough is not used."
                        : "DNG payload mode")

                    Button {
                        manager.sendToResolve(item: item, specificClipFolder: clip.folderName)
                    } label: {
                        Image(systemName: "plus.rectangle.on.folder")
                    }
                    .buttonStyle(.borderless)
                    .help("Import only this clip into DaVinci Resolve Media Pool")
                    .disabled(manager.isImportingToResolve)
                    .frame(width: 24)
                }
                .font(.system(size: 12))
                .monospacedDigit()
                .padding(.vertical, 5)
                .background(idx % 2 == 1 ? Color.primary.opacity(0.03) : Color.clear)
                .contextMenu {
                    // Keyboard / VoiceOver path to what the preview click does.
                    Button("Play") {
                        PlayerWindows.shared.open(item: item, clipIndex: idx, title: clip.folderName)
                    }
                    Button("Import to DaVinci Resolve") {
                        manager.sendToResolve(item: item, specificClipFolder: clip.folderName)
                    }
                    .disabled(manager.isImportingToResolve)
                    Button("Show in Finder") {
                        manager.revealInFinder(path: clipPath(clip.folderName))
                    }
                }

                if idx < item.summary.clips.count - 1 {
                    Divider().opacity(0.4).gridCellColumns(10)
                }
            }
        }
    }
}

struct MountCardView: View {
    @ObservedObject var item: ActiveMountItem
    @ObservedObject var manager = MountManager.shared

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            HStack {
                Image(systemName: "externaldrive.fill")
                    .font(.system(size: 17))
                    .foregroundStyle(.secondary)

                VStack(alignment: .leading, spacing: 2) {
                    Text(URL(fileURLWithPath: item.summary.mountPoint).lastPathComponent)
                        .font(.system(size: 14, weight: .semibold))
                    Text("\(item.summary.mountPoint)  •  NFSv3 127.0.0.1:\(item.summary.nfsPort)  •  Indexed in \(String(format: "%.1f", item.summary.indexTimeMs)) ms")
                        .font(.system(size: 11))
                        .foregroundStyle(.secondary)
                        .lineLimit(1)
                        .truncationMode(.middle)
                }

                Spacer()

                Button {
                    manager.revealInFinder(path: item.summary.mountPoint)
                } label: {
                    Label("Finder", systemImage: "folder")
                }
                .buttonStyle(.bordered)
                .controlSize(.small)

                Button {
                    manager.sendToResolve(item: item)
                } label: {
                    Label("Import to Resolve", systemImage: "film")
                }
                .buttonStyle(.borderedProminent)
                .tint(.orange)
                .controlSize(.small)
                .disabled(manager.isImportingToResolve)

                Button(role: .destructive) {
                    manager.unmount(item: item)
                } label: {
                    Label("Unmount", systemImage: "eject.fill")
                }
                .buttonStyle(.bordered)
                .controlSize(.small)
            }

            Divider()

            ClipTableView(item: item)
        }
        .padding(12)
        .background(
            RoundedRectangle(cornerRadius: 8)
                .fill(Color(nsColor: .controlBackgroundColor))
        )
        .overlay(
            RoundedRectangle(cornerRadius: 8)
                .stroke(Color.primary.opacity(0.08), lineWidth: 1)
        )
    }
}

/// Card for a mount that is still being indexed: overall progress, then one row per clip file
/// with its state (waiting / indexing / ready / failed). Mirrors the Windows app.
struct PendingMountCardView: View {
    @ObservedObject var pending: PendingMount

    private var done: Int {
        pending.clips.filter { $0.state != .waiting && $0.state != .loading }.count
    }

    private var headline: String {
        if pending.attaching { return "Mounting the volume…" }
        if pending.clips.isEmpty { return "Looking for clips…" }
        let failed = pending.failedNames.count
        return "Indexing clips… \(done) / \(pending.clips.count)" + (failed > 0 ? "   (\(failed) failed)" : "")
    }

    private func look(_ state: PendingMount.ClipState) -> (String, String, Color) {
        switch state {
        case .waiting: return ("clock", "Waiting", .secondary)
        case .loading: return ("arrow.triangle.2.circlepath", "Indexing…", .orange)
        case .ready: return ("checkmark.circle.fill", "Ready", .green)
        case .failed(let msg): return ("exclamationmark.triangle.fill", "Failed: \(msg)", .red)
        }
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            HStack(spacing: 10) {
                ProgressView().controlSize(.small)
                VStack(alignment: .leading, spacing: 2) {
                    Text(pending.title)
                        .font(.system(size: 14, weight: .semibold))
                        .lineLimit(1)
                        .truncationMode(.middle)
                    Text(headline)
                        .font(.system(size: 11))
                        .foregroundStyle(.orange)
                }
                Spacer()
            }
            ProgressView(value: Double(done), total: Double(max(pending.clips.count, 1)))
                .tint(.orange)
            if !pending.clips.isEmpty {
                Divider()
                VStack(alignment: .leading, spacing: 4) {
                    ForEach(pending.clips) { clip in
                        let (icon, label, color) = look(clip.state)
                        HStack(spacing: 8) {
                            Image(systemName: icon)
                                .foregroundStyle(color)
                                .frame(width: 16)
                            Text(clip.name)
                                .font(.system(size: 12, design: .monospaced))
                                .foregroundStyle(clip.state == .waiting ? .secondary : .primary)
                                .lineLimit(1)
                                .truncationMode(.middle)
                            Spacer()
                            Text(label)
                                .font(.system(size: 11))
                                .foregroundStyle(color)
                                .lineLimit(1)
                                .frame(maxWidth: 260, alignment: .trailing)
                        }
                    }
                }
            }
        }
        .padding(12)
        .background(
            RoundedRectangle(cornerRadius: 8)
                .fill(Color(nsColor: .controlBackgroundColor))
        )
        .overlay(
            RoundedRectangle(cornerRadius: 8)
                .stroke(Color.primary.opacity(0.08), lineWidth: 1)
        )
    }
}

struct MenuBarDropdownView: View {
    @ObservedObject var manager = MountManager.shared
    @Environment(\.openWindow) private var openWindow

    var body: some View {
        Button("Open CineFS Window...") {
            NSApp.activate(ignoringOtherApps: true)
            openWindow(id: "main")
        }
        .keyboardShortcut("o", modifiers: [.command])

        Button("Add Clip or Folder...") {
            manager.promptAddDialog()
        }
        .keyboardShortcut("a", modifiers: [.command])

        Button("Select DaVinci Resolve.app...") {
            manager.promptChooseResolveApp()
        }

        Divider()

        if manager.mounts.isEmpty {
            Text("No Active Mounts")
                .foregroundStyle(.secondary)
        } else {
            ForEach(manager.mounts) { item in
                let name = URL(fileURLWithPath: item.summary.mountPoint).lastPathComponent
                Menu("\(name) (\(item.summary.clips.count) clip\(item.summary.clips.count == 1 ? "" : "s"))") {
                    Button("Show in Finder") {
                        manager.revealInFinder(path: item.summary.mountPoint)
                    }
                    Button("Import All Clips to DaVinci Resolve") {
                        manager.sendToResolve(item: item)
                    }
                    Divider()
                    Button("Unmount") {
                        manager.unmount(item: item)
                    }
                }
            }
            Divider()
            Button("Unmount All") {
                manager.unmountAllSync()
            }
        }

        Divider()

        Button("Quit CineFS") {
            manager.unmountAllSync()
            NSApplication.shared.terminate(nil)
        }
        .keyboardShortcut("q", modifiers: [.command])
    }
}

@main
struct CineFSApp: App {
    @NSApplicationDelegateAdaptor(AppDelegate.self) var appDelegate
    @ObservedObject private var manager = MountManager.shared

    var body: some Scene {
        Window("CineFS", id: "main") {
            MainMountWindowView()
        }
        .defaultSize(width: 740, height: 480)

        MenuBarExtra {
            MenuBarDropdownView()
        } label: {
            HStack(spacing: 4) {
                Image(systemName: "film.stack")
                if !manager.mounts.isEmpty {
                    Text("\(manager.mounts.count)")
                }
            }
        }
    }
}
