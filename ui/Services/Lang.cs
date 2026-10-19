using System;
using System.Globalization;
using Windows.Storage;

namespace Synka.Services;

/// <summary>The language choice in the settings.</summary>
public enum AppLanguage
{
    Swedish,
    English,
}

/// <summary>
/// The language of the window: Swedish by default, English on request.
///
/// <para>
/// The texts stand as TWO STRINGS IN PLACE — <c>Lang.T("Översikt", "Overview")</c>
/// in code and <c>{loc:T Sv='Översikt', En='Overview'}</c> in XAML — instead of
/// keys against a resource file. Whoever rewrites a wording sees both languages
/// at once, and a key catalogue cannot quietly leave one of them behind.
/// </para>
///
/// <para>
/// The engine's own texts (status line, notifications, activity details) are
/// Swedish and are shown as they are. The window translates the engine's state
/// codes itself.
/// </para>
/// </summary>
public static class Lang
{
    private const string LanguageKey = "AppLanguage";

    /// <summary>Decimal comma and ISO weekdays — the Swedish conventions.</summary>
    private static readonly CultureInfo SwedishCulture = CultureInfo.GetCultureInfo("sv-SE");

    /// <summary>en-GB and not en-US: the app is metric, and 2026-08-14 must not read as 8 April.</summary>
    private static readonly CultureInfo EnglishCulture = CultureInfo.GetCultureInfo("en-GB");

    private static AppLanguage _current = LoadSaved();

    /// <summary>Raised when the language has changed, so the window can redraw.</summary>
    public static event EventHandler? Changed;

    public static AppLanguage Current => _current;

    /// <summary>True when English applies.</summary>
    public static bool IsEnglish => _current == AppLanguage.English;

    /// <summary>The culture that applies to numbers and dates in UI texts.</summary>
    public static CultureInfo Culture => IsEnglish ? EnglishCulture : SwedishCulture;

    /// <summary>The sentence in the language that applies.</summary>
    public static string T(string sv, string en) => IsEnglish ? en : sv;

    /// <summary>Saves the choice and applies it at once. Choosing what already applies does nothing.</summary>
    public static void Change(AppLanguage language)
    {
        if (language == _current)
        {
            return;
        }

        _current = language;
        Save(language);
        Changed?.Invoke(null, EventArgs.Empty);
    }

    /// <summary>
    /// The saved choice, or Swedish. The default is Swedish on purpose: the app
    /// is built for Swedish projects, and the choice can be changed in Inställningar.
    /// </summary>
    private static AppLanguage LoadSaved()
    {
        try
        {
            if (ApplicationData.Current.LocalSettings.Values[LanguageKey] is string saved
                && Enum.TryParse(saved, out AppLanguage stored))
            {
                return stored;
            }
        }
        catch (Exception)
        {
            // Without package identity there is no storage. Swedish applies.
        }

        return AppLanguage.Swedish;
    }

    private static void Save(AppLanguage language)
    {
        try
        {
            ApplicationData.Current.LocalSettings.Values[LanguageKey] = language.ToString();
        }
        catch (Exception)
        {
            // A choice that could not be saved still applies until the window closes.
        }
    }
}
