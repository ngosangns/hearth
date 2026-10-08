import SwiftUI
import HearthKit

struct SidebarView: View {
    @Environment(AppModel.self) private var model

    var body: some View {
        VStack(spacing: 0) {
            PaneHeader(title: model.pane.rawValue, systemImage: nil) {
                WordmarkLogo(markSize: 20)
            }
            SidebarToolbar()
            ZStack(alignment: .top) {
                Divider()
                if model.isBusy { ProgressView().progressViewStyle(.linear).accessibilityLabel("Working") }
            }
            .frame(height: 3)
            switch model.pane {
            case .workspaces: WorkspaceList()
            case .shared: SharedList()
            }
        }
        .background(.background)
    }
}

/// Pane switcher and labeled action buttons.
struct SidebarToolbar: View {
    @Environment(AppModel.self) private var model

    var body: some View {
        @Bindable var model = model
        VStack(alignment: .leading, spacing: Theme.Space.sm) {
            Picker("Pane", selection: $model.pane) {
                Label("Workspaces", systemImage: Icon.folder).tag(Pane.workspaces)
                Label("Shared", systemImage: Icon.shared).tag(Pane.shared)
            }
            .pickerStyle(.segmented)
            .labelsHidden()
            .help("Switch between project workspaces and shared services")
            .accessibilityLabel("Pane")

            VStack(spacing: Theme.Space.sm) {
                if model.pane == .workspaces {
                    Button { model.chooseFolder() } label: {
                        Label("Add folder", systemImage: Icon.addFolder).frame(maxWidth: .infinity)
                    }
                    .buttonStyle(.borderedProminent)
                    .help("Add a project folder that has a hearth.yaml")
                }
                Button { model.refresh() } label: {
                    Label {
                        Text(model.isBusy ? "Reloading…" : "Reload")
                    } icon: {
                        ZStack {
                            if model.isBusy { ProgressView().controlSize(.small) } else { Image(systemName: Icon.reload) }
                        }
                        .frame(width: 16, height: 16)
                    }
                    .frame(maxWidth: .infinity)
                }
                .buttonStyle(.bordered)
                .disabled(model.isBusy)
                .help(model.pane == .workspaces ? "Reload the workspace list and daemon status" : "Reload shared recipes and instances")
            }
            .controlSize(.regular)
        }
        .padding(.horizontal, Theme.Space.md)
        .padding(.vertical, Theme.Space.sm)
    }
}

/// Workspaces, pinned to the top of the column.
struct WorkspaceList: View {
    @Environment(AppModel.self) private var model

    var body: some View {
        if model.workspaces.isEmpty {
            StateView(systemImage: Icon.folder, title: "No workspaces yet",
                      message: "Add a project folder that has a hearth.yaml.") {
                Button { model.chooseFolder() } label: { Label("Add folder", systemImage: Icon.addFolder) }
                    .buttonStyle(.borderedProminent)
            }
            .frame(maxHeight: .infinity, alignment: .top)
        } else {
            ScrollView {
                LazyVStack(alignment: .leading, spacing: 2) {
                    SectionHeading(title: "Workspaces", count: model.workspaces.count)
                        .padding(.horizontal, Theme.Space.sm).padding(.bottom, Theme.Space.xs)
                    ForEach(model.workspaces) { item in
                        SelectableRow(selected: item.id == model.selectedId) {
                            model.selectWorkspace(item.id)
                        } content: {
                            WorkspaceRow(item: item)
                        }
                        .contextMenu {
                            Button { model.selectWorkspace(item.id); model.requestForget() } label: {
                                Label("Forget", systemImage: Icon.remove)
                            }
                            Button { Finder.reveal(item.path) } label: { Label("Reveal in Finder", systemImage: Icon.reveal) }
                                .disabled(item.missing)
                            Button { Finder.copy(item.path) } label: { Label("Copy path", systemImage: Icon.copy) }
                        }
                    }
                }
                .padding(Theme.Space.sm)
            }
            .frame(maxHeight: .infinity, alignment: .top)
        }
    }
}

struct WorkspaceRow: View {
    let item: WorkspaceItem

    var body: some View {
        HStack(alignment: .top, spacing: Theme.Space.sm) {
            Image(systemName: item.missing ? Icon.workspaceMissing : Icon.folder)
                .foregroundStyle(item.missing ? .red : .secondary).accessibilityHidden(true)
            VStack(alignment: .leading, spacing: 1) {
                Text(item.name).lineLimit(1).truncationMode(.middle)
                Text(item.displayPath).font(.caption).foregroundStyle(.secondary)
                    .lineLimit(1).truncationMode(.head)
            }
            Spacer(minLength: Theme.Space.xs)
            if item.missing {
                Tag(text: "missing", tint: .red)
            } else if !item.trusted {
                Tag(text: "untrusted", tint: .orange)
            } else if item.stopped {
                Tag(text: "stopped")
            } else if item.attached {
                Circle().fill(.green).frame(width: 7, height: 7).padding(.top, 5)
                    .help("Daemon attached").accessibilityLabel("Daemon attached")
            }
        }
        .accessibilityElement(children: .combine)
    }
}

/// Shared instances and recipes, in the same list shape as workspaces.
struct SharedList: View {
    @Environment(AppModel.self) private var model

    var body: some View {
        if !model.sharedLoaded {
            WorkingView(message: "Loading shared services…").frame(maxHeight: .infinity, alignment: .top)
        } else {
            ScrollView {
                VStack(alignment: .leading, spacing: 2) {
                    SectionHeading(title: "Instances", count: model.instances.count)
                        .padding(.horizontal, Theme.Space.sm).padding(.bottom, Theme.Space.xs)
                    if model.instances.isEmpty {
                        Text("No instances.").font(.callout).foregroundStyle(.secondary).padding(.horizontal, Theme.Space.sm)
                    }
                    ForEach(model.instances) { instance in
                        SelectableRow(selected: model.sharedSelection == .instance(instance.id)) {
                            model.sharedSelection = .instance(instance.id)
                        } content: {
                            HStack(spacing: Theme.Space.sm) {
                                Image(systemName: Icon.shared).foregroundStyle(.secondary).accessibilityHidden(true)
                                Text(instance.id).lineLimit(1).truncationMode(.middle)
                                Spacer(minLength: Theme.Space.xs)
                                if let state = instance.actualState, !state.isEmpty {
                                    StatePill(state: state, label: ServiceBoard.compactWire(state))
                                }
                            }
                        }
                    }
                    SectionHeading(title: "Recipes", count: model.recipes.count)
                        .padding(.horizontal, Theme.Space.sm).padding(.vertical, Theme.Space.xs).padding(.top, Theme.Space.sm)
                    ForEach(model.recipes) { recipe in
                        SelectableRow(selected: model.sharedSelection == .recipe(recipe.id)) {
                            model.sharedSelection = .recipe(recipe.id)
                        } content: {
                            HStack(spacing: Theme.Space.sm) {
                                Image(systemName: Icon.recipe).foregroundStyle(.secondary).accessibilityHidden(true)
                                Text(recipe.id).lineLimit(1).truncationMode(.middle)
                                Spacer(minLength: 0)
                            }
                        }
                    }
                }
                .padding(Theme.Space.sm)
            }
            .frame(maxHeight: .infinity, alignment: .top)
        }
    }
}
