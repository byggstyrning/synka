using System;
using System.Collections.Generic;
using System.Globalization;
using System.Linq;
using System.Text;
using System.Text.Json;
using System.Text.RegularExpressions;

namespace Synka.Services;

/// <summary>Safe user-facing summaries and per-item reasons for engine errors.</summary>
public static class ErrorDisplay
{
    private const int MaxInput = 8192;
    private const int MaxMessage = 500;
    private const RegexOptions SafeRegex = RegexOptions.IgnoreCase | RegexOptions.CultureInvariant | RegexOptions.NonBacktracking;
    private static readonly TimeSpan RegexTimeout = TimeSpan.FromMilliseconds(100);
    private static readonly Regex HttpStatus = new(@"\bHTTP\s+([1-5][0-9]{2})\b", SafeRegex, RegexTimeout);
    private static readonly Regex Url = new("""\b(?:https?|ftp|file)://[^\s<>"']+""", SafeRegex, RegexTimeout);
    private static readonly Regex Authorization = new(@"\b(?:proxy-)?authorization\s*[:=][^\r\n]*", SafeRegex, RegexTimeout);
    // Keep each non-backtracking automaton below .NET's size limit.
    private static readonly Regex[] Credentials =
    {
        CredentialPattern("access[_-]?token|refresh[_-]?token|id[_-]?token|session[_-]?token|token|client[_-]?secret|api[_-]?key"),
        CredentialPattern("password|passwd|pwd|secret|signature|authorization|cookie|sig|code[_-]?verifier|authorization[_-]?code|auth[_-]?code|code"),
        CredentialPattern("x-amz-signature|x-amz-credential|x-amz-security-token|x-goog-signature|x-goog-credential|aws[_-]?access[_-]?key[_-]?id|aws[_-]?secret[_-]?access[_-]?key"),
    };
    private static readonly Regex AuthValue = new(@"\b(?:Bearer|Basic)\s+[A-Za-z0-9._~+/=-]+", SafeRegex, RegexTimeout);
    private static readonly Regex Jwt = new(@"\beyJ[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+\b", SafeRegex, RegexTimeout);
    // Spaces, punctuation and apostrophes can belong to Windows path components.
    // Hide ambiguous unquoted trailing text through the line instead of exposing a path tail.
    private static readonly Regex LocalPath = new("""'?(?:\b[A-Z]:[\\/]|\\\\)[^\r\n<>"|]*""", SafeRegex, RegexTimeout);
    private static readonly Regex Spaces = new(@"\s+", SafeRegex, RegexTimeout);

    public static readonly Uri AccFileTypesHelp = new("https://help.autodesk.com/view/DOCS/ENU/?guid=Supported_Files_Docs");

    public static bool TryUnsupportedFileType(string? raw, out string extension)
    {
        extension = string.Empty;
        if (string.IsNullOrWhiteSpace(raw)) return false;

        const string prefix = "File with extension ";
        const string suffix = " is not allowed to upload";
        var start = raw.IndexOf(prefix, StringComparison.OrdinalIgnoreCase);
        if (start < 0) return false;
        start += prefix.Length;
        var end = raw.IndexOf(suffix, start, StringComparison.OrdinalIgnoreCase);
        if (end <= start) return false;

        var candidate = raw[start..end].Trim().TrimStart('.');
        if (candidate.Length is < 1 or > 12 || !candidate.All(char.IsLetterOrDigit)) return false;
        extension = "." + candidate.ToLowerInvariant();
        return true;
    }

    public static string FriendlyDetail(string? raw)
    {
        if (TryUnsupportedFileType(raw, out var extension))
        {
            return Lang.T(
                $"ACC tar inte emot {extension}-filer. Filen finns kvar på datorn.",
                $"ACC does not accept {extension} files. The file remains on this computer.");
        }

        if (string.IsNullOrWhiteSpace(raw)) return string.Empty;
        if (raw.Contains("HTTP ", StringComparison.OrdinalIgnoreCase) || raw.Contains("{\"jsonapi\"", StringComparison.Ordinal))
        {
            return Lang.T(
                "Synkningen gick inte att slutföra. Se loggen för detaljer.",
                "Sync could not finish. See the log for details.");
        }
        return SafeMessage(raw);
    }

    /// <summary>Read only known error fields, never a response body, links, headers or credentials.</summary>
    public static string FailureDetail(string? raw)
    {
        try { return FailureDetailCore(raw); }
        catch (RegexMatchTimeoutException) { return NoReason(); }
    }

    private static string FailureDetailCore(string? raw)
    {
        if (string.IsNullOrWhiteSpace(raw)) return NoReason();
        if (TryUnsupportedFileType(raw, out _)) return FriendlyDetail(raw);
        var bounded = raw.Length <= MaxInput ? raw : raw[..MaxInput];
        var statusMatch = HttpStatus.Match(bounded);
        var status = statusMatch.Success ? statusMatch.Groups[1].Value : string.Empty;
        var parts = new List<string>();
        var jsonStart = bounded.IndexOfAny(new[] { '{', '[' });
        var parsed = false;
        if (jsonStart >= 0 && raw.Length <= MaxInput)
        {
            try
            {
                // Parse the JSON value while allowing the engine's surrounding error context.
                var reader = new Utf8JsonReader(Encoding.UTF8.GetBytes(bounded[jsonStart..]));
                using var document = JsonDocument.ParseValue(ref reader);
                var root = document.RootElement;
                var errors = Property(root, "errors");
                if (errors.ValueKind == JsonValueKind.Array)
                {
                    foreach (var error in errors.EnumerateArray().Take(3)) AddReason(error, parts);
                }
                else if (errors.ValueKind == JsonValueKind.Object) AddReason(errors, parts);
                else AddReason(root, parts);
                parsed = true;
            }
            catch (JsonException)
            {
                // Truncated or malformed response bodies are not safe text to display.
            }
        }
        var plain = statusMatch.Success ? bounded[(statusMatch.Index + statusMatch.Length)..].TrimStart(' ', ':') : bounded;
        if (!parsed && jsonStart < 0 && !plain.TrimStart().StartsWith("<", StringComparison.Ordinal))
        {
            var message = SafeMessage(plain);
            if (message.Length > 0) parts.Add(message);
        }
        if (status.Length > 0 && !parts.Any(part => part.StartsWith("HTTP " + status, StringComparison.OrdinalIgnoreCase)))
            parts.Insert(0, "HTTP " + status);
        if (parts.Count == 0 || parts.All(part => part == "HTTP " + status)) parts.Add(NoReason());
        if (parts.Any(part => part.Contains("VER_MISMATCH_MIMETYPE", StringComparison.OrdinalIgnoreCase)))
        {
            parts.Add(Lang.T(
                "ACC kräver samma MIME-typ som förra versionen. Kontrollera att filformatet är detsamma. Om formatet ska ändras, ladda upp filen som en ny fil i ACC. För Revit-molnmodeller eller delade modeller kan vanlig filversionsuppladdning sakna stöd; synkronisera och publicera sådana modeller från Revit.",
                "ACC requires the same MIME type as the previous version. Check that the file format is the same. If the format is intentionally different, upload it as a new file in ACC. For Revit cloud/workshared models, ordinary binary version uploads may be unsupported; synchronize and publish those models from Revit."));
        }
        else if (status is "401" or "403")
        {
            parts.Add(Lang.T("Kontrollera inloggningen och att du får ändra filen i ACC.",
                "Check your sign-in and permission to change this file in ACC."));
        }
        else if (status is "429" or "500" or "502" or "503" or "504")
        {
            parts.Add(Lang.T("Försök igen när ACC är tillgängligt.", "Retry when ACC is available."));
        }
        return string.Join("\n\n", parts.Distinct(StringComparer.Ordinal));
    }

    private static void AddReason(JsonElement error, List<string> parts)
    {
        if (error.ValueKind != JsonValueKind.Object) return;
        var title = Text(error, "title");
        var detail = Text(error, "detail");
        if (detail.Length == 0) detail = Text(error, "message");
        if (detail.Length == 0) detail = Text(error, "userMessage");
        if (detail.Length == 0) detail = Text(error, "developerMessage");
        if (detail.Length == 0) detail = Text(error, "error_description");
        var code = Text(error, "code");
        if (code.Length == 0) code = Text(error, "errorCode");
        if (code.Length == 0) code = Text(error, "error");
        var lines = new[] { title, detail, code.Length == 0 ? string.Empty : Lang.T("Felkod: ", "Error code: ") + code };
        var reason = string.Join("\n", lines.Where(line => line.Length > 0).Distinct(StringComparer.Ordinal));
        if (reason.Length > 0) parts.Add(reason);
    }

    private static JsonElement Property(JsonElement value, string name)
    {
        if (value.ValueKind == JsonValueKind.Object)
            foreach (var property in value.EnumerateObject())
                if (property.Name.Equals(name, StringComparison.OrdinalIgnoreCase)) return property.Value;
        return default;
    }

    private static string Text(JsonElement value, string name)
    {
        var property = Property(value, name);
        return property.ValueKind == JsonValueKind.String ? SafeMessage(property.GetString() ?? string.Empty) : string.Empty;
    }

    private static Regex CredentialPattern(string names) => new(
        "\\b(?<key>" + names + ")[\"']?\\s*[:=]\\s*(?:\"[^\"\\r\\n]*\"|'[^'\\r\\n]*'|[^\\s,;]+)",
        SafeRegex, RegexTimeout);
    private static string SafeMessage(string text)
    {
        try { return SafeMessageCore(text); }
        catch (RegexMatchTimeoutException) { return NoReason(); }
    }

    private static string SafeMessageCore(string text)
    {
        if (text.Length > MaxInput) text = text[..MaxInput];
        text = Url.Replace(text, Lang.T("[URL dold]", "[URL removed]"));
        text = Authorization.Replace(text, Lang.T("[autentisering dold]", "[authorization removed]"));
        foreach (var credential in Credentials) text = credential.Replace(text, "${key}: [redacted]");
        text = AuthValue.Replace(text, "[redacted]");
        text = Jwt.Replace(text, "[redacted]");
        text = LocalPath.Replace(text, Lang.T("[lokal sökväg dold]", "[local path removed]"));
        var visible = new string(text.Select(character => char.IsControl(character)
            || char.GetUnicodeCategory(character) == UnicodeCategory.Format ? ' ' : character).ToArray());
        visible = Spaces.Replace(visible, " ").Trim();
        return visible.Length <= MaxMessage ? visible : visible[..MaxMessage] + " …";
    }

    private static string NoReason() => Lang.T(
        "Motorn gav ingen läsbar felorsak. Öppna loggmappen för fler detaljer.",
        "The engine supplied no readable reason. Open the log folder for more details.");
}
