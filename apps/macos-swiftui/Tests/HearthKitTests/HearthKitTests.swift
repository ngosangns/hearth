import Foundation
import Testing
@testable import HearthKit

private func service(_ id: String, readiness: String = "http", label: String? = nil, disabled: Bool = false,
                     argv: [String] = ["/bin/echo"], kind: String? = nil, ports: [Int] = []) -> CatalogService {
    CatalogService(id: id, label: label, kind: kind, disabled: disabled, readinessKind: readiness, argv: argv, ports: ports)
}

@Suite struct CommandResultTests {
    @Test func prettyDocumentWinsOverNestedOneLineObject() {
        let stdout = """
        {
            "instances": [
                {
                    "name": "redis",
                    "attachments": {
                        "abc": {"projectRoot": "/work/viclass", "provisioned": true}
                    }
                }
            ]
        }
        """
        let decoded = CommandResult(ok: true, exit: 0, stdout: stdout, stderr: "").decode(SharedInstances.self)
        #expect(decoded?.instances.first?.name == "redis")
        #expect(decoded?.instances.first?.attachmentRoots == ["/work/viclass"])
    }

    @Test func jsonLineAfterALogLineStillDecodes() {
        let out = "listening\n{\"port\": 7, \"token\": \"secret\"}\n"
        let info = CommandResult(ok: true, exit: 0, stdout: out, stderr: "").decode(DaemonInfo.self)
        #expect(info?.port == 7)
        #expect(Session(info!)?.token == "secret")
    }

    @Test func tokensAreRedactedFromVisibleMessages() {
        let r = CommandResult(ok: false, exit: 1, stdout: "", stderr: "bad {\"token\": \"abc123\"}")
        #expect(!r.visibleMessage.contains("abc123"))
        #expect(r.visibleMessage.contains("[redacted]"))
    }

    @Test func sessionNeedsTokenAndPort() {
        #expect(Session(DaemonInfo(port: 9, token: nil, protocolVersion: 3, instanceId: nil)) == nil)
        #expect(Session(DaemonInfo(port: 0, token: "t", protocolVersion: nil, instanceId: nil)) == nil)
    }
}

@Suite struct CLIRunnerTests {
    @Test func runsWithArgvAndReportsExitAndStderr() async {
        let cli = HearthCLI(executable: URL(fileURLWithPath: "/bin/sh"))
        let ok = await cli.run(["-c", "echo $0 hi", "x y"], cwd: "/")
        #expect(ok.ok && ok.stdout.trimmingCharacters(in: .whitespacesAndNewlines) == "x y hi")
        let bad = await cli.run(["-c", "echo oops >&2; exit 3"], cwd: "/")
        #expect(!bad.ok && bad.exit == 3 && bad.visibleMessage == "oops")
    }

    @Test func timeoutKillsTheChild() async {
        let cli = HearthCLI(executable: URL(fileURLWithPath: "/bin/sleep"))
        let r = await cli.run(["30"], cwd: "/", timeout: .milliseconds(200))
        #expect(!r.ok)
        #expect(r.stderr == "command timed out")
    }

    @Test func missingBinaryFailsWithoutThrowing() async {
        let cli = HearthCLI(executable: URL(fileURLWithPath: "/nonexistent/hearth"))
        let r = await cli.run(["--version"], cwd: "/")
        #expect(!r.ok && !r.visibleMessage.isEmpty)
    }

    @Test func managerTimeoutsFollowTheSubcommand() {
        #expect(HearthCLI.managerTimeout("ensure") == .seconds(90))
        #expect(HearthCLI.managerTimeout("stop") == .seconds(300))
        #expect(HearthCLI.managerTimeout("status") == .seconds(20))
    }
}

@Suite struct ManagerClientTests {
    let session = Session(token: "tok", port: 4242, protocolVersion: 3)

    @Test func requestCarriesBearerAndProtocol() throws {
        let r = try ManagerClient(session: session).makeRequest("GET", "/v1/logs/a%20b", query: ["limit": "10"])
        #expect(r.url?.absoluteString == "http://127.0.0.1:4242/v1/logs/a%20b?limit=10")
        #expect(r.value(forHTTPHeaderField: "Authorization") == "Bearer tok")
        #expect(r.value(forHTTPHeaderField: "x-hearth-protocol") == "3")
    }

    @Test func serviceIdsAreEscapedAsOnePathSegment() {
        #expect(ManagerClient.escape("a/b c") == "a%2Fb%20c")
    }

    @Test func killUnownedOnlyOnStart() throws {
        let start = try ManagerClient.operationBody(serviceId: "api", action: "start", killUnowned: true, requestId: "r")
        let json = try JSONSerialization.jsonObject(with: start) as? [String: Any]
        #expect(json?["killUnowned"] as? Bool == true)
        #expect(throws: ManagerError.self) { try ManagerClient.operationBody(serviceId: "api", action: "stop", killUnowned: true) }
        let plain = try ManagerClient.operationBody(serviceId: "api", action: "stop", killUnowned: false)
        #expect((try JSONSerialization.jsonObject(with: plain) as? [String: Any])?["killUnowned"] == nil)
    }

    @Test func daemonErrorEnvelopeIsReadable() {
        let data = Data(#"{"error":{"code":"port_held","message":"Port 80 is held by pid 5"}}"#.utf8)
        #expect(ManagerClient.errorMessage(data) == "Port 80 is held by pid 5")
        #expect(ManagerClient.errorMessage(Data("nope".utf8)) == "")
    }
}

@Suite struct WireDecodingTests {
    @Test func catalogDecodesProfilesPortsAndSharedAttach() throws {
        let json = """
        {"catalog":{"services":[
          {"id":"api","label":"rust api","kind":"infrastructure","disabled":false,
           "profiles":{"run":{"commandStatus":"verified","command":{"command":{"argv":["x"]}},"readiness":{"kind":"exit"}}},
           "ports":[{"port":49144,"label":"api"}, 8080, "9090"]},
          {"id":"db","profiles":{"run":{"commandStatus":"verified","command":{"command":{"argv":["hearth","shared","attach","postgres@16.4"]}},"readiness":{"kind":"command"}}}}
        ],"groups":{"all":["api","db"]},"groupTree":[{"name":"app","members":["api"]}]}}
        """
        let doc = try JSONDecoder().decode(CatalogEnvelope.self, from: Data(json.utf8)).catalog
        #expect(doc.services[0].ports == [49144, 8080, 9090])
        #expect(doc.services[0].readinessKind == "exit")
        #expect(doc.services[1].sharedInstance == "postgres@16.4")
        #expect(doc.services[0].sharedInstance == nil)
        #expect(doc.groups["all"] == ["api", "db"])
    }

    @Test func unverifiedCommandIsNeverShared() throws {
        let json = #"{"id":"db","profiles":{"run":{"commandStatus":"unverified","command":{"command":{"argv":["hearth","shared","attach","pg@1"]}}}}}"#
        #expect(try JSONDecoder().decode(CatalogService.self, from: Data(json.utf8)).sharedInstance == nil)
    }

    @Test func sharedInstancesAcceptMapOrListAttachments() throws {
        let map = #"{"instances":[{"name":"redis","version":"8","port":43001,"installState":"installed","state":{"actualState":"ready"},"attachments":{"b":{"projectRoot":"/b"},"a":{"projectRoot":"/a"}}}]}"#
        let list = #"{"instances":[{"name":"redis","version":"8","attachments":[{"projectRoot":"/a"}]}]}"#
        let a = try JSONDecoder().decode(SharedInstances.self, from: Data(map.utf8)).instances[0]
        #expect(a.id == "redis@8" && a.isUp && a.port == 43001)
        #expect(a.attachmentRoots == ["/a", "/b"])
        #expect(try JSONDecoder().decode(SharedInstances.self, from: Data(list.utf8)).instances[0].attachmentRoots == ["/a"])
    }

    @Test func recipesListEveryVersion() throws {
        let json = #"{"version":1,"services":{"redis":{"versions":{"8.2.10":{},"7.0":{}}},"mongodb":{"versions":{"8.0.32":{}}}}}"#
        let ids = try JSONDecoder().decode(SharedCatalog.self, from: Data(json.utf8)).recipes.map(\.id)
        #expect(ids == ["mongodb@8.0.32", "redis@7.0", "redis@8.2.10"])
    }
}

@Suite struct ServiceBoardTests {
    func catalog() -> Catalog {
        Catalog(
            services: [
                service("web"), service("job", readiness: "exit", label: "Job"), service("off", disabled: true),
                service("db", readiness: "command", label: "Database", argv: ["hearth", "shared", "attach", "postgres@16.4"]),
            ],
            groups: ["all": ["web", "job", "db"]],
            groupTree: [.init(name: "app", members: ["web", "job"]), .init(name: "data", members: ["db", "off"])]
        )
    }
    let live = [
        LiveService(serviceId: "web", actualState: "running-unready"),
        LiveService(serviceId: "job", actualState: "succeeded"),
        LiveService(serviceId: "db", actualState: "ready"),
        LiveService(serviceId: "off", actualState: "stopped"),
    ]

    @Test func sectionsFollowDirectMembershipAndDegradeUnready() {
        let sections = ServiceBoard.sections(catalog: catalog(), live: live)
        #expect(sections.map(\.name) == ["app", "data"])
        #expect(sections[0].services[0].display == "degraded")
        #expect(sections[0].services[0].up)
        #expect(sections[0].services[1].finite)
        // Inside a group, rows follow catalog order (off is declared before db).
        #expect(sections[1].services.map(\.id) == ["off", "db"])
        #expect(sections[1].services[1].sharedInstance == "postgres@16.4")
        #expect(ServiceBoard.summary(sections) == "2/3 ready")
        #expect(ServiceBoard.stopAllTargets(sections) == ["web", "db"])
        #expect(ServiceBoard.startAllTargets(groups: catalog().groups, sections: sections) == ["web", "job", "db"])
        #expect(ServiceBoard.groupIsUp(sections, name: "app"))
        #expect(ServiceBoard.groupTargets(sections, name: "data") == ["db"])
    }

    @Test func serviceInTwoGroupsSitsUnderTheFirst() {
        let cat = Catalog(services: [service("a")], groupTree: [.init(name: "x", members: ["a"]), .init(name: "y", members: ["a"])])
        let sections = ServiceBoard.sections(catalog: cat, live: [])
        #expect(sections.map(\.name) == ["x"])
    }

    @Test func ungroupedServicesTrailInAnUnnamedSection() {
        let cat = Catalog(services: [service("a"), service("b")], groupTree: [.init(name: "x", members: ["a"])])
        let sections = ServiceBoard.sections(catalog: cat, live: [])
        #expect(sections.map(\.name) == ["x", nil])
        #expect(sections[1].services.map(\.id) == ["b"])
    }

    @Test func aLiveRowMissingFromTheCatalogStillShows() {
        let sections = ServiceBoard.sections(catalog: Catalog(), live: [LiveService(serviceId: "ghost", actualState: "ready")])
        #expect(sections[0].services.map(\.id) == ["ghost"])
    }

    @Test func failedFiniteServicesCountAndSucceededOnesDoNot() {
        let cat = Catalog(services: [service("job", readiness: "exit"), service("export", readiness: "exit"), service("web")])
        let sections = ServiceBoard.sections(catalog: cat, live: [
            LiveService(serviceId: "job", actualState: "failed"),
            LiveService(serviceId: "export", actualState: "succeeded"),
            LiveService(serviceId: "web", actualState: "stopped"),
        ])
        #expect(ServiceBoard.summary(sections) == "0/2 ready  1 failed")
        #expect(!ServiceBoard.showsStop("succeeded"))
        #expect(ServiceBoard.showsStop("starting") && ServiceBoard.showsStop("ready"))
        let urls = ServiceBoard.visibleUrls([
            UrlRow(serviceId: "export", url: "http://x/export", label: "Export"),
            UrlRow(serviceId: "web", url: "http://x/web"),
            UrlRow(serviceId: "web", url: "http://x/always", requiresRunning: false),
        ], sections: sections)
        #expect(urls.map(\.url) == ["http://x/export", "http://x/always"])
        #expect(urls[0].label == "Export")
    }

    @Test func catalogReloadWaitsForASecondObservation() {
        #expect(!ServiceBoard.shouldReloadCatalog(previous: nil, next: 10))
        #expect(!ServiceBoard.shouldReloadCatalog(previous: 10, next: 10))
        #expect(!ServiceBoard.shouldReloadCatalog(previous: 10, next: nil))
        #expect(ServiceBoard.shouldReloadCatalog(previous: 10, next: 11))
    }

    @Test func sharedNoticesNameWorkspaces() {
        let notice = ServiceBoard.projectSharedNotice(
            "stop", current: "desk", touches: [.init(instance: "postgres@16.4", others: ["viclass"])], unknown: [])
        #expect(notice.contains("postgres@16.4 (viclass)"))
        #expect(notice.contains("only detaches desk"))
        #expect(notice.hasSuffix("Those workspaces keep it."))
        let remove = ServiceBoard.instanceSharedNotice("remove", instance: "redis@8", affected: ["infra", "viclass"])
        #expect(remove.contains("deletes its data") && remove.contains("infra and viclass"))
        #expect(ServiceBoard.joinNames(["a", "b", "c"]) == "a, b, and c")
    }

    @Test func attachmentsOutsideTheCurrentWorkspaceAreOthers() {
        let known = [ServiceBoard.WorkspaceLabel(root: "/work/a", name: "alpha")]
        let report = ServiceBoard.classifyAttachments(["/work/a", "/work/b"], current: "/work/a", known: known)
        #expect(report.all == ["alpha", "b"])
        #expect(report.others == ["b"])
        #expect(ServiceBoard.classifyAttachments(["/work/a"], current: nil, known: known).others == ["alpha"])
    }

    @Test func logTailCountsCharacters() {
        #expect(ServiceBoard.boundedTail("ééé", limit: 2) == "éé")
        #expect(ServiceBoard.boundedTail("abc", limit: 0) == "")
    }
}

@Suite struct LogBufferTests {
    @Test func resetReplacesAndAppendAccumulates() {
        var log = LogBuffer()
        log.apply(LogSlice(data: "one\n", nextCursor: 4, generation: 1_000_000, reset: true))
        log.apply(LogSlice(data: "two\n", nextCursor: 8, generation: 1_000_000, reset: false))
        #expect(log.text == "one\ntwo\n" && log.cursor == 8 && log.generation == 1_000_000)
        log.apply(LogSlice(data: "fresh\n", nextCursor: 6, generation: 2_000_000, reset: true))
        #expect(log.text == "fresh\n")
    }

    @Test func aLifecycleChangeDropsTheCursor() {
        var log = LogBuffer()
        log.observe(liveGeneration: 2)
        log.apply(LogSlice(data: "x", nextCursor: 1, generation: 2_000_000, reset: true))
        log.observe(liveGeneration: 2)
        #expect(log.cursor == 1)
        log.observe(liveGeneration: 3)
        #expect(log.cursor == nil && log.generation == nil)
    }

    @Test func earlierGrowsTheWindowOnlyWhenMoreExists() {
        var log = LogBuffer()
        let first = log.expand()
        #expect(!first)
        log.apply(LogSlice(data: String(repeating: "x", count: LogBuffer.defaultLimit), nextCursor: 1, generation: 0, reset: true))
        #expect(log.hasMore)
        let second = log.expand()
        #expect(second)
        #expect(log.limit == LogBuffer.defaultLimit * 4 && log.text.isEmpty && log.cursor == nil)
    }

    @Test func escapeSequencesAreStrippedForDisplay() {
        let raw = "\u{1B}[31mred\u{1B}[0m plain \u{1B}]0;title\u{07}ok"
        #expect(LogBuffer.stripEscapes(raw) == "red plain ok")
        #expect(LogBuffer.stripEscapes("no escapes") == "no escapes")
    }

    @Test func identicalTextDoesNotBumpTheRevision() {
        var log = LogBuffer()
        log.apply(LogSlice(data: "a", nextCursor: 1, generation: 0, reset: true))
        let before = log.revision
        log.apply(LogSlice(data: "a", nextCursor: 1, generation: 0, reset: true))
        #expect(log.revision == before)
    }
}

@Suite struct WorkspaceStoreTests {
    func tempFile() -> String {
        let dir = NSTemporaryDirectory() + "hearth-ws-\(UUID().uuidString)"
        return dir + "/workspaces.json"
    }

    func folder() throws -> String {
        let dir = NSTemporaryDirectory() + "hearth-folder-\(UUID().uuidString)"
        try FileManager.default.createDirectory(atPath: dir, withIntermediateDirectories: true)
        return (dir as NSString).resolvingSymlinksInPath
    }

    @Test func addTrustRemoveRoundTripUsesTheTuiShape() throws {
        let file = tempFile()
        let store = WorkspaceStore(path: file)
        let dir = try folder()
        let added = try store.add(dir)
        #expect(added.created && !added.record.trusted)
        #expect(added.record.id == added.record.id.uppercased())
        #expect(!added.record.addedAt.contains("."))
        #expect(try store.add(dir).created == false)
        try store.trust(added.record.id)

        let reopened = WorkspaceStore(path: file)
        #expect(reopened.rows == [WorkspaceRecord(id: added.record.id, path: dir, trusted: true, addedAt: added.record.addedAt)])
        #expect(try reopened.remove(added.record.id))
        #expect(WorkspaceStore(path: file).rows.isEmpty)
    }

    @Test func rejectsRelativeAndMissingFolders() {
        let store = WorkspaceStore(path: tempFile())
        #expect(throws: WorkspaceError.self) { try store.add("relative/path") }
        #expect(throws: WorkspaceError.self) { try store.add("/definitely/not/here-\(UUID().uuidString)") }
    }

    @Test func aCorruptFileIsMovedAsideOnOpenButKeptOnReload() throws {
        let file = tempFile()
        try FileManager.default.createDirectory(atPath: (file as NSString).deletingLastPathComponent, withIntermediateDirectories: true)
        try Data("not json".utf8).write(to: URL(fileURLWithPath: file))
        let store = WorkspaceStore(path: file)
        #expect(store.rows.isEmpty && store.loadError != nil)
        #expect(!FileManager.default.fileExists(atPath: file))

        let dir = try folder()
        let added = try store.add(dir)
        try Data("garbage".utf8).write(to: URL(fileURLWithPath: file))
        #expect(store.reload() != nil)
        #expect(store.rows.map(\.id) == [added.record.id])
        #expect(FileManager.default.fileExists(atPath: file))
    }

    @Test func displayPathAbbreviatesHome() {
        #expect(WorkspaceStore.displayPath("/Users/me/x", home: "/Users/me") == "~/x")
        #expect(WorkspaceStore.displayPath("/Users/me", home: "/Users/me") == "~")
        #expect(WorkspaceStore.displayPath("/Users/meow/x", home: "/Users/me") == "/Users/meow/x")
        #expect(WorkspaceStore.folderName("/a/b") == "b")
    }
}
