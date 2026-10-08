import SwiftUI

/// Design tokens. Prefer system colors/materials so the app follows
/// light/dark mode, accent color and Increase Contrast automatically.
enum Theme {
    enum Space {
        static let xs: CGFloat = 4
        static let sm: CGFloat = 8
        static let md: CGFloat = 12
        static let lg: CGFloat = 16
        static let xl: CGFloat = 24
    }
    enum Radius {
        static let sm: CGFloat = 6
        static let md: CGFloat = 10
    }
    /// Brand accent (teal → indigo), used by the logo and hero badges.
    static let brandA = Color(red: 0.13, green: 0.77, blue: 0.69)
    static let brandB = Color(red: 0.36, green: 0.36, blue: 0.93)
    static var brandGradient: LinearGradient {
        LinearGradient(colors: [brandA, brandB], startPoint: .topLeading, endPoint: .bottomTrailing)
    }
}
