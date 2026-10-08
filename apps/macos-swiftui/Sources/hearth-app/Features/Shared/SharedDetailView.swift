import SwiftUI
import HearthKit

/// Second column of the Shared pane, laid out like a workspace: hero card, then sections.
struct SharedDetailView: View {
    @Environment(AppModel.self) private var model

    var body: some View {
        VStack(spacing: 0) {
            PaneHeader(title: title, subtitle: model.smpLive ? "smp live" : "local registry", systemImage: Icon.shared)
            if !model.sharedLoaded {
                WorkingView(message: "Loading shared services…")
            } else {
                switch model.sharedSelection {
                case .instance(let id):
                    if let instance = model.instances.first(where: { $0.id == id }) { InstanceDetail(instance: instance) } else { empty }
                case .recipe(let id):
                    if let recipe = model.recipes.first(where: { $0.id == id }) { RecipeDetail(recipe: recipe) } else { empty }
                case nil:
                    empty
                }
            }
        }
    }

    private var title: String {
        switch model.sharedSelection {
        case .instance(let id), .recipe(let id): id
        case nil: "Shared services"
        }
    }

    private var empty: some View {
        StateView(systemImage: Icon.sparkles, title: "Select a shared service",
                  message: "Pick an instance or recipe from the sidebar.") { LogoMark(size: 56) }
    }
}

private struct InstanceDetail: View {
    @Environment(AppModel.self) private var model
    let instance: SharedInstance

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: Theme.Space.lg) {
                Card {
                    VStack(alignment: .leading, spacing: Theme.Space.md) {
                        HeroHeader(systemImage: Icon.shared, title: instance.id, subtitle: "Shared instance") {
                            if let state = instance.actualState, !state.isEmpty {
                                StatePill(state: state, label: ServiceBoard.compactWire(state))
                            }
                            if let port = instance.port { Tag(text: "port \(port)", tint: .blue) }
                            if !instance.installState.isEmpty { Tag(text: instance.installState) }
                        }
                        ActionBar(isBusy: model.isBusy) {
                            if instance.isUp {
                                Button { model.requestStopInstance(instance.id) } label: { Label("Stop", systemImage: Icon.stop) }
                                    .help("Stop \(instance.id) for every attached workspace")
                            } else {
                                Button { model.startInstance(instance.id) } label: { Label("Start", systemImage: Icon.play) }
                                    .buttonStyle(.borderedProminent)
                                    .help("Start \(instance.id)")
                            }
                            Button { model.requestRestartInstance(instance.id) } label: { Label("Restart", systemImage: Icon.restart) }
                                .help("Restart \(instance.id)")
                            Button(role: .destructive) { model.requestRemoveInstance(instance.id) } label: {
                                Label("Remove", systemImage: Icon.remove)
                            }
                            .help("Delete \(instance.id) and its data")
                        }
                    }
                }
                VStack(alignment: .leading, spacing: Theme.Space.sm) {
                    SectionHeading(title: "Attached workspaces", count: instance.attachmentRoots.count)
                    if instance.attachmentRoots.isEmpty {
                        Card { Text("No workspace is attached.").font(.callout).foregroundStyle(.secondary) }
                    }
                    ForEach(instance.attachmentRoots, id: \.self) { root in
                        Card {
                            HStack(spacing: Theme.Space.sm) {
                                Image(systemName: Icon.folder).foregroundStyle(.secondary)
                                VStack(alignment: .leading, spacing: 1) {
                                    Text(WorkspaceStore.folderName(root)).font(.headline)
                                    Text(WorkspaceStore.displayPath(root)).font(.caption).foregroundStyle(.secondary)
                                }
                                Spacer()
                                Button { Finder.reveal(root) } label: { Label("Reveal", systemImage: Icon.reveal) }
                                    .buttonStyle(.bordered).help("Reveal \(root) in Finder")
                            }
                        }
                    }
                }
            }
            .frame(maxWidth: .infinity, alignment: .leading)
            .padding(Theme.Space.lg)
        }
    }
}

private struct RecipeDetail: View {
    @Environment(AppModel.self) private var model
    let recipe: SharedCatalog.Recipe

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: Theme.Space.lg) {
                Card {
                    VStack(alignment: .leading, spacing: Theme.Space.md) {
                        HeroHeader(systemImage: Icon.recipe, title: recipe.id, subtitle: "Recipe from the shared catalog") {
                            Tag(text: recipe.version, tint: .blue)
                            if installed { Tag(text: "installed", tint: .green) }
                        }
                        ActionBar(isBusy: model.isBusy) {
                            Button { model.installRecipe(recipe.id) } label: { Label(installed ? "Reinstall" : "Install", systemImage: Icon.install) }
                                .buttonStyle(.borderedProminent)
                                .help("Install \(recipe.id)")
                        }
                    }
                }
            }
            .frame(maxWidth: .infinity, alignment: .leading)
            .padding(Theme.Space.lg)
        }
    }

    private var installed: Bool { model.instances.contains { $0.id == recipe.id } }
}
