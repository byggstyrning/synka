using System;

namespace Synka.Services;

/// <summary>
/// The document providers (docs\PROVIDERS.md) as the window names them: the ids the
/// engine sends, the short labels for tags and prefixes, and the signed-in wording.
/// </summary>
public static class ProviderNames
{
    public const string Acc = "acc";
    public const string StreamBim = "streambim";
    public const string Interaxo = "interaxo";

    /// <summary>The provider id. A hub or account without one is ACC (an older engine).</summary>
    public static string Id(string? provider) => string.IsNullOrWhiteSpace(provider) ? Acc : provider;

    /// <summary>The short name for tags and prefixes: "ACC", "StreamBIM", "Interaxo".</summary>
    public static string Label(string? provider) => Id(provider) switch
    {
        StreamBim => "StreamBIM",
        Interaxo => "Interaxo",
        Acc => "ACC",
        var other => other,
    };

    /// <summary>The provider's full name, as the account card shows it.</summary>
    public static string Title(string provider) => Id(provider) switch
    {
        StreamBim => "StreamBIM",
        Interaxo => "Interaxo",
        _ => "Autodesk Construction Cloud (ACC)",
    };

    /// <summary>"Anna Andersson (anna@firma.se)": the name and the e-mail the engine has. Empty when it has neither.</summary>
    public static string Who(UserInfo? user)
    {
        var name = user?.Name?.Trim();
        var email = user?.Email?.Trim();
        if (string.IsNullOrEmpty(name))
        {
            return email ?? string.Empty;
        }

        return string.IsNullOrEmpty(email) ? name : $"{name} ({email})";
    }
}
