using System;
using System.Collections.Generic;
using System.Linq;

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

    /// <summary>
    /// The labels of these providers in a sentence: "ACC", "ACC och StreamBIM", "ACC, StreamBIM och
    /// Interaxo" (English "and"); with <paramref name="or"/>, "eller" ("or") instead.
    /// </summary>
    public static string Sentence(IReadOnlyList<string> providers, bool or = false)
    {
        var labels = providers.Select(Label).ToList();
        if (labels.Count < 2)
        {
            return labels.FirstOrDefault() ?? string.Empty;
        }

        var last = or ? Lang.T(" eller ", " or ") : Lang.T(" och ", " and ");
        return string.Join(", ", labels.Take(labels.Count - 1)) + last + labels[^1];
    }

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

    /// <summary>
    /// The hub-style name of a StreamBIM account, as its hub carries it (docs\PROVIDERS.md):
    /// "StreamBIM" for the default region, "StreamBIM &lt;title&gt;" for another region (the title from
    /// the region list), "StreamBIM (&lt;host&gt;)" for a custom address or a region the list does not name.
    /// </summary>
    public static string StreamBimName(AccountInfo? account, StreamBimRegionsInfo? regions)
    {
        var key = account?.Account ?? string.Empty;
        var host = string.IsNullOrWhiteSpace(account?.Host) ? key : account!.Host!;
        if (key.Length == 0 || key == "app")
        {
            return Title(StreamBim);
        }
        if (key.Contains('.'))
        {
            return $"StreamBIM ({host})";
        }

        var title = regions?.Regions?.FirstOrDefault(region => region is not null && region.Subdomain == key)?.Title;
        return string.IsNullOrWhiteSpace(title) ? $"StreamBIM ({host})" : "StreamBIM " + title.Trim();
    }

    /// <summary>One account's name in a list: the hub-style name for each StreamBIM account, the provider label otherwise.</summary>
    public static string AccountLabel(AccountInfo account, StreamBimRegionsInfo? regions) =>
        Id(account.Provider) == StreamBim ? StreamBimName(account, regions) : Label(account.Provider);

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
