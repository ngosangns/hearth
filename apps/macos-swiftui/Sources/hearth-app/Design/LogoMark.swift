import SwiftUI

/// Hearth logo: a white flame on synca's brand-gradient squircle. `scripts/make-icon.swift`
/// renders the same artwork into `AppIcon.icns`; keep the two in step.
struct LogoMark: View {
    var size: CGFloat = 28

    var body: some View {
        ZStack {
            RoundedRectangle(cornerRadius: size * 0.2237, style: .continuous).fill(Theme.brandGradient)
            RoundedRectangle(cornerRadius: size * 0.2237, style: .continuous)
                .fill(LinearGradient(colors: [.white.opacity(0.28), .white.opacity(0)],
                                     startPoint: .top, endPoint: UnitPoint(x: 0.5, y: 0.55)))
            Image(systemName: Icon.flameFilled)
                .font(.system(size: size * 0.56))
                .foregroundStyle(.white)
                .shadow(color: .black.opacity(0.28), radius: size * 0.02, y: size * 0.012)
        }
        .frame(width: size, height: size)
        .accessibilityElement()
        .accessibilityLabel("Hearth")
    }
}

/// Mark plus the wordmark, used in the sidebar header.
struct WordmarkLogo: View {
    var markSize: CGFloat = 28

    var body: some View {
        HStack(spacing: Theme.Space.sm) {
            LogoMark(size: markSize)
            Text("hearth")
                .font(.system(.title3, design: .rounded, weight: .semibold))
                .foregroundStyle(.primary)
        }
        .accessibilityElement(children: .ignore)
        .accessibilityLabel("Hearth")
    }
}
