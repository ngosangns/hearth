import AppKit

extension BoardViewController {

    /// Semantic color for a wire state, matching the PHP pills.
    func stateColor(_ state: String) -> NSColor {
        switch state {
        case "ready", "succeeded": return .systemGreen
        case "running", "running-unready": return .systemBlue
        case "starting", "preparing", "queued-start", "stopping": return .systemOrange
        case "failed", "orphaned", "externally-owned": return .systemRed
        default: return .secondaryLabelColor
        }
    }

    /// The Shared pane: recipe catalog + installed instances.
    func buildShared(_ s: DeskController.Snapshot) {
        stackAdd(makeLabel("Shared", size: 17, weight: .bold))
        stackAdd(makeLabel(
            s.smpLive ? "Live smp." : "Local registry. Drawing does not start smp.",
            size: 12, color: .secondaryLabelColor))
        if let notice = s.notice {
            stackAdd(NoticeView(notice))
        }
        stackAdd(CallbackButton(
            title: "Refresh", symbol: Symbols.restart,
            onClick: { [weak self] in self?.desk.refreshShared() }))

        stackAdd(sharedHeader("Recipes"))
        if s.recipes.isEmpty {
            stackAdd(makeLabel("No recipes.", size: 12, color: .secondaryLabelColor))
        } else {
            for recipe in s.recipes {
                stackAdd(recipeRow(recipe))
            }
        }

        stackAdd(sharedHeader("Instances"))
        if s.instances.isEmpty {
            stackAdd(makeLabel("No instances.", size: 12, color: .secondaryLabelColor))
        } else {
            for instance in s.instances {
                stackAdd(instanceRow(instance, snapshot: s))
            }
        }
    }

    private func sharedHeader(_ title: String) -> NSView {
        let label = makeLabel(title.uppercased(), size: 10, weight: .bold, color: .secondaryLabelColor)
        let wrap = NSView()
        wrap.addSubview(label)
        label.translatesAutoresizingMaskIntoConstraints = false
        NSLayoutConstraint.activate([
            label.leadingAnchor.constraint(equalTo: wrap.leadingAnchor),
            label.topAnchor.constraint(equalTo: wrap.topAnchor, constant: 12),
            label.bottomAnchor.constraint(equalTo: wrap.bottomAnchor),
        ])
        return wrap
    }

    private func rowCard(icon: String, title: String, subtitle: String, actions: [NSView]) -> NSView {
        let card = NSView()
        card.wantsLayer = true
        card.layer?.cornerRadius = 10
        card.layer?.backgroundColor = NSColor.controlBackgroundColor.cgColor
        card.layer?.borderWidth = 0.5
        card.layer?.borderColor = NSColor.separatorColor.cgColor

        let iconView = NSImageView(image: symbolImage(icon, 14) ?? NSImage())
        iconView.contentTintColor = .secondaryLabelColor
        let name = makeLabel(title, size: 13, weight: .semibold)
        let sub = makeLabel(subtitle, size: 11, color: .secondaryLabelColor)
        sub.maximumNumberOfLines = 0
        sub.lineBreakMode = .byWordWrapping
        let text = NSStackView(views: [name, sub])
        text.orientation = .vertical
        text.alignment = .leading
        text.spacing = 1

        let left = NSStackView(views: [iconView, text])
        left.spacing = 8
        left.alignment = .top

        let row = NSStackView()
        row.orientation = .horizontal
        row.alignment = .centerY
        row.distribution = .gravityAreas
        row.edgeInsets = NSEdgeInsets(top: 8, left: 10, bottom: 8, right: 10)
        row.addArrangedSubview(left)
        let actionsStack = NSStackView(views: actions)
        actionsStack.spacing = 6
        actionsStack.orientation = .horizontal
        row.addArrangedSubview(actionsStack)
        left.setContentHuggingPriority(.defaultLow, for: .horizontal)
        actionsStack.setContentHuggingPriority(.required, for: .horizontal)

        row.translatesAutoresizingMaskIntoConstraints = false
        card.addSubview(row)
        NSLayoutConstraint.activate([
            row.leadingAnchor.constraint(equalTo: card.leadingAnchor),
            row.trailingAnchor.constraint(equalTo: card.trailingAnchor),
            row.topAnchor.constraint(equalTo: card.topAnchor),
            row.bottomAnchor.constraint(equalTo: card.bottomAnchor),
        ])
        return card
    }

    private func recipeRow(_ recipe: ServiceBoard.Recipe) -> NSView {
        rowCard(
            icon: Symbols.cube,
            title: recipe.name,
            subtitle: recipe.version,
            actions: [CallbackButton(
                title: "Install", symbol: Symbols.download,
                onClick: { [weak self] in self?.desk.installRecipe(recipe.id) })]
        )
    }

    private func instanceRow(_ instance: ServiceBoard.Instance, snapshot s: DeskController.Snapshot) -> NSView {
        var sub = instance.installState
        if !instance.display.isEmpty { sub += " · \(instance.display)" }
        if let port = instance.port { sub += " · port \(port)" }
        sub += " · \(instance.attachments) attached"

        let up = ["ready", "running", "running-unready"].contains(instance.state)
        var actions: [NSView] = []
        if up {
            let armed = s.pendingKind == "instance-stop" && s.pendingId == instance.id
            actions.append(CallbackButton(
                title: armed ? "Confirm stop" : "Stop",
                symbol: Symbols.stop, kind: armed ? .armedDanger : .normal,
                onClick: { [weak self] in self?.desk.stopInstance(instance.id) }))
        } else {
            actions.append(CallbackButton(
                title: "Start", symbol: Symbols.play, kind: .primary,
                onClick: { [weak self] in self?.desk.startInstance(instance.id) }))
        }
        let restartArmed = s.pendingKind == "instance-restart" && s.pendingId == instance.id
        actions.append(CallbackButton(
            title: restartArmed ? "Confirm restart" : "Restart",
            symbol: Symbols.restart, kind: restartArmed ? .armed : .normal,
            onClick: { [weak self] in self?.desk.restartInstance(instance.id) }))
        let removeArmed = s.pendingKind == "shared-remove" && s.pendingId == instance.id
        actions.append(CallbackButton(
            title: removeArmed ? "Confirm remove" : "Remove",
            symbol: Symbols.trash, kind: removeArmed ? .armedDanger : .normal,
            onClick: { [weak self] in self?.desk.removeInstance(instance.id) }))
        return rowCard(icon: Symbols.drives, title: instance.id, subtitle: sub, actions: actions)
    }
}
