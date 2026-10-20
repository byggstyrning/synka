using System;
using System.Linq;

namespace Synka.Services;

/// <summary>
/// The custom StreamBIM address the window checks before it sends one (docs\IPC.md, `sign_in`):
/// https only, a DNS host with a dot, and no port, path, query, fragment or user info. A single
/// trailing slash is the root and is dropped. The engine checks the address again and stays the
/// authority; this check only says why an address cannot be used before anything is sent.
/// </summary>
public static class StreamBimAddress
{
    private const string Scheme = "https://";
    private const string StreamBimSuffix = ".streambim.com";

    /// <summary>A checked address: the URL `sign_in` sends, its host, and the account key the engine derives from it.</summary>
    public readonly record struct Parsed(string Url, string Host, string Key);

    /// <summary>
    /// Checks the typed address. On failure <paramref name="error"/> says what is wrong, in the
    /// window's language. A `*.streambim.com` host has the region key (its subdomain), as the engine.
    /// </summary>
    public static bool TryParse(string? text, out Parsed address, out string? error)
    {
        address = default;
        error = null;

        var value = (text ?? string.Empty).Trim();
        if (!value.StartsWith(Scheme, StringComparison.OrdinalIgnoreCase))
        {
            error = Lang.T("Adressen måste börja med https://.", "The address must start with https://.");
            return false;
        }

        var host = value[Scheme.Length..];
        if (host.EndsWith('/'))
        {
            host = host[..^1];
        }
        host = host.ToLowerInvariant();

        if (!IsDnsNameWithDot(host))
        {
            error = Lang.T(
                "Skriv bara värdnamnet, till exempel https://streambim.exempel.se, utan port, sökväg eller frågetecken.",
                "Enter only the host name, for example https://streambim.example.se, without a port, path or query.");
            return false;
        }

        address = new Parsed("https://" + host, host, KeyFor(host));
        return true;
    }

    /// <summary>The account key of a host: the subdomain for `*.streambim.com` (a region), the host otherwise.</summary>
    public static string KeyFor(string host)
    {
        if (host.EndsWith(StreamBimSuffix, StringComparison.Ordinal))
        {
            var subdomain = host[..^StreamBimSuffix.Length];
            if (subdomain.Length > 0 && !subdomain.Contains('.'))
            {
                return subdomain;
            }
        }
        return host;
    }

    /// <summary>Two or more ASCII labels of letters, digits and hyphens, none starting or ending with a hyphen; the last label has a letter (so an IP address does not pass).</summary>
    private static bool IsDnsNameWithDot(string host)
    {
        if (host.Length is 0 or > 253)
        {
            return false;
        }

        var labels = host.Split('.');
        if (labels.Length < 2)
        {
            return false;
        }

        foreach (var label in labels)
        {
            if (label.Length is 0 or > 63 || label[0] == '-' || label[^1] == '-')
            {
                return false;
            }
            if (!label.All(character => character is (>= 'a' and <= 'z') or (>= '0' and <= '9') or '-'))
            {
                return false;
            }
        }

        return labels[^1].Any(character => character is >= 'a' and <= 'z');
    }
}
