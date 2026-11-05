//! The public texts say the same true things: every data statement names the same platforms and
//! says there is no telemetry, the trademark notice is word for word the same everywhere it is
//! shown, and no text describes Synka as for Byggstyrning's staff only or names an earlier
//! product name. Whitespace is compared collapsed, so line wrapping does not matter.

macro_rules! file {
    ($path:literal) => {
        (
            $path,
            include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/", $path)),
        )
    };
}

const README: (&str, &str) = file!("README.md");
const PRIVACY: (&str, &str) = file!("docs/PRIVACY.md");
const ROADMAP: (&str, &str) = file!("docs/ROADMAP.md");
const MARKETPLACE: (&str, &str) = file!("docs/MARKETPLACE.md");
const DESIGN: (&str, &str) = file!("DESIGN.md");
const PACKAGING: (&str, &str) = file!("packaging/README.md");
const DELIVERY: (&str, &str) = file!("packaging/delivery/README.txt");
const INSTALL: (&str, &str) = file!("packaging/delivery/Install.ps1");
const NEW_RELEASE: (&str, &str) = file!("packaging/new_release.ps1");
const RELEASE_YML: (&str, &str) = file!(".github/workflows/release.yml");
const ABOUT: (&str, &str) = file!("ui/Views/AboutPage.xaml");
const ABOUT_CODE: (&str, &str) = file!("ui/Views/AboutPage.xaml.cs");
const SETTINGS: (&str, &str) = file!("ui/Views/SettingsPage.xaml");

const PLATFORMS: [&str; 3] = ["Autodesk", "StreamBIM", "Interaxo"];

const TRADEMARK_EN: &str = "Autodesk, Autodesk Construction Cloud, Autodesk Docs, Autodesk Forma \
    and Revit are registered trademarks or trademarks of Autodesk, Inc., and/or its subsidiaries \
    and/or affiliates. Synka is an independent product of Byggstyrning and is not affiliated with, \
    endorsed by or sponsored by Autodesk.";
const TRADEMARK_SV: &str = "Autodesk, Autodesk Construction Cloud, Autodesk Docs, Autodesk Forma \
    och Revit är registrerade varumärken eller varumärken som tillhör Autodesk, Inc. och/eller \
    dess dotter- eller närstående bolag. Synka är en fristående produkt från Byggstyrning och är \
    inte ansluten till, godkänd av eller sponsrad av Autodesk.";

/// Runs of whitespace (line breaks included) as one space.
fn collapsed(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The text from `start` up to the next `end` after it.
fn section<'a>((path, text): (&str, &'a str), start: &str, end: &str) -> &'a str {
    let from = text
        .find(start)
        .unwrap_or_else(|| panic!("{path}: no {start:?}"));
    let rest = &text[from + start.len()..];
    let to = rest
        .find(end)
        .unwrap_or_else(|| panic!("{path}: no {end:?} after {start:?}"));
    &rest[..to]
}

#[test]
fn every_data_statement_names_the_same_platforms_and_no_telemetry() {
    let statements = [
        (
            "README.md, Use and data",
            section(README, "## Use and data", "\n## "),
        ),
        (
            "docs/PRIVACY.md, Who receives data",
            section(PRIVACY, "## Who receives data", "\n## "),
        ),
        (
            "AboutPage.xaml, privacy card",
            section(ABOUT, "x:Name=\"PrivacyCard\"", ">"),
        ),
        (
            "delivery README.txt, VIKTIGT",
            section(DELIVERY, "VIKTIGT", "KRAV"),
        ),
    ];
    for (place, text) in statements {
        for platform in PLATFORMS {
            assert!(text.contains(platform), "{place} does not name {platform}");
        }
        let lower = text.to_lowercase();
        assert!(
            lower.contains("telemetr"),
            "{place} does not say there is no telemetry"
        );
    }
}

#[test]
fn the_acc_only_about_text_names_autodesk_alone() {
    let text = section(
        ABOUT_CODE,
        "PrivacyCard.Description = accOnly",
        ": _privacyDescription",
    );
    assert!(text.contains("Autodesk"));
    for other in ["StreamBIM", "Interaxo"] {
        assert!(
            !text.contains(other),
            "the ACC-only privacy text names {other}"
        );
    }
}

#[test]
fn the_trademark_notice_is_word_for_word_the_same() {
    for (path, text) in [README, ABOUT, NEW_RELEASE, RELEASE_YML] {
        assert!(
            collapsed(text).contains(TRADEMARK_EN),
            "{path}: the English trademark notice differs"
        );
    }
    for (path, text) in [ABOUT, DELIVERY, NEW_RELEASE, RELEASE_YML] {
        assert!(
            collapsed(text).contains(TRADEMARK_SV),
            "{path}: the Swedish trademark notice differs"
        );
    }
}

#[test]
fn no_text_describes_a_staff_only_distribution() {
    const STAFF_ONLY: [&str; 9] = [
        "internal use",
        "internal only",
        "shared internally",
        "internal share",
        "internal onedrive",
        "no public download",
        "internt bruk",
        "delas internt",
        "interna onedrive",
    ];
    for (path, text) in [
        README,
        PRIVACY,
        ROADMAP,
        MARKETPLACE,
        DESIGN,
        PACKAGING,
        DELIVERY,
        NEW_RELEASE,
        RELEASE_YML,
        ABOUT,
        SETTINGS,
    ] {
        let lower = collapsed(text).to_lowercase();
        for phrase in STAFF_ONLY {
            assert!(!lower.contains(phrase), "{path} says {phrase:?}");
        }
    }
}

#[test]
fn earlier_product_names_appear_only_as_package_identities() {
    for (path, text) in [
        README,
        ROADMAP,
        MARKETPLACE,
        PACKAGING,
        DELIVERY,
        INSTALL,
        RELEASE_YML,
    ] {
        let text = collapsed(text);
        assert!(
            !text.contains("Better Desktop Connector"),
            "{path} names an earlier product name"
        );
    }
    // Install.ps1 still needs the identities to find and remove an earlier package.
    assert!(INSTALL
        .1
        .contains("\"Byggstyrning.BetterDesktopConnector\""));
}
