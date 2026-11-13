using System;
using System.Collections.Generic;
using System.Collections.ObjectModel;
using System.Globalization;
using System.Linq;
using Synka.Services;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Data;

namespace Synka.Views;

/// <summary>
/// Formatting shared by the pages. Numbers and dates follow the app language
/// (<see cref="Lang.Culture"/>): Swedish uses a decimal comma ("12,3 MB") and "för
/// 3 min sedan"; English (en-GB) uses a decimal point and "3 min ago". A language
/// change reloads the pages, which is enough to switch the formatting too.
/// </summary>
public static class Fmt
{
    private static readonly string[] Units = { "B", "KB", "MB", "GB", "TB" };

    /// <summary>The largest Unix millisecond DateTimeOffset accepts (9999-12-31).</summary>
    private const long MaxUnixMs = 253_402_300_799_999;

    public static bool IsSwedish => !Lang.IsEnglish;

    /// <summary>The culture of the app language, the same one <see cref="Lang"/> uses.</summary>
    public static CultureInfo Culture => Lang.Culture;

    /// <summary>Bytes as "512 B", "12,3 MB", "2,1 GB". Null shows an en dash.</summary>
    public static string Bytes(long? bytes)
    {
        if (bytes is null)
        {
            return "–";
        }

        var (value, unit) = Split(bytes.Value);
        return $"{Num(value, unit)} {Units[unit]}";
    }

    /// <summary>
    /// "12,3 av 80,1 MB": both numbers in the unit of the total, so the unit is
    /// said once. Used for transfer progress. An unknown total shows only what is done.
    /// </summary>
    public static string Pair(long done, long total)
    {
        if (total <= 0)
        {
            return Bytes(Math.Max(0, done));
        }

        var (_, unit) = Split(total);
        var scale = Math.Pow(1024, unit);
        var doneText = Num(Math.Clamp(done, 0, total) / scale, unit);
        var totalText = Num(total / scale, unit);
        return $"{doneText} {Lang.T("av", "of")} {totalText} {Units[unit]}";
    }

    /// <summary>"42 %" (Swedish) or "42%" (English). Empty when the total is unknown.</summary>
    public static string Percent(long done, long total) =>
        total > 0 ? ((double)Math.Clamp(done, 0, total) / total).ToString("P0", Culture) : string.Empty;

    /// <summary>"2,1 MB/s".</summary>
    public static string Speed(double bytesPerSecond) =>
        $"{Bytes((long)Math.Round(Math.Max(0, bytesPerSecond)))}/s";

    /// <summary>A whole number with the language's digit grouping: "1 234" or "1,234".</summary>
    public static string Count(long value) => value.ToString("N0", Culture);

    /// <summary>Relative time for an engine timestamp (Unix milliseconds). Empty for a time that cannot be read.</summary>
    public static string Ago(long tsMs)
    {
        if (!TryLocal(tsMs, out var when))
        {
            return string.Empty;
        }

        var now = DateTimeOffset.Now;
        var span = now - when;

        if (span < TimeSpan.FromMinutes(1))
        {
            return Lang.T("just nu", "just now");
        }

        if (span < TimeSpan.FromHours(1))
        {
            var m = (int)span.TotalMinutes;
            return Lang.T($"för {m} min sedan", $"{m} min ago");
        }

        if (span < TimeSpan.FromDays(1))
        {
            var h = (int)span.TotalHours;
            return Lang.T($"för {h} tim sedan", $"{h} h ago");
        }

        if (span < TimeSpan.FromDays(7))
        {
            var d = (int)span.TotalDays;
            return Lang.T(
                $"för {d} {(d == 1 ? "dag" : "dagar")} sedan",
                $"{d} {(d == 1 ? "day" : "days")} ago");
        }

        return when.ToString(when.Year == now.Year ? "d MMM HH:mm" : "d MMM yyyy HH:mm", Culture);
    }

    /// <summary>Date and time for an engine timestamp, in the language's short form. Empty when unreadable.</summary>
    public static string Absolute(long tsMs) =>
        TryLocal(tsMs, out var when) ? when.ToString("g", Culture) : string.Empty;

    /// <summary>Segoe Fluent glyph for an activity kind. Kinds the window does not know get the info glyph.</summary>
    public static string KindGlyph(string? kind) => kind switch
    {
        "download" => "",
        "upload" => "",
        "new_version" => "",
        "rename" => "",
        "move" => "",
        "delete" => "",
        "conflict" => "",
        "error" => "",
        "unsupported" => "",
        "remote_change" => "",
        "free_up" => "",
        "lock" => "",
        "unlock" => "",
        "skipped" => "",
        "reset" => "",
        "restore" => "",
        "held" => "",
        _ => "",
    };

    /// <summary>Accessible name and tooltip text for an activity kind. Kinds the window does not know read as "Händelse".</summary>
    public static string KindLabel(string? kind) => kind switch
    {
        "download" => Lang.T("Hämtad", "Downloaded"),
        "upload" => Lang.T("Uppladdad", "Uploaded"),
        "new_version" => Lang.T("Ny version", "New version"),
        "rename" => Lang.T("Namnbyte", "Renamed"),
        "move" => Lang.T("Flyttad", "Moved"),
        "delete" => Lang.T("Borttagen", "Deleted"),
        "conflict" => Lang.T("Konflikt", "Conflict"),
        "error" => Lang.T("Fel", "Error"),
        "unsupported" => Lang.T("Filtyp stöds inte", "Unsupported file type"),
        "remote_change" => Lang.T("Ändrad i molnet", "Changed in the cloud"),
        "free_up" => Lang.T("Utrymme frigjort", "Space freed"),
        "lock" => Lang.T("Låst", "Locked"),
        "unlock" => Lang.T("Upplåst", "Unlocked"),
        "skipped" => Lang.T("Hoppades över", "Skipped"),
        "reset" => Lang.T("Ombyggd från molnet", "Rebuilt from the cloud"),
        "restore" => Lang.T("Återställd", "Restored"),
        "held" => Lang.T("Väntar på bekräftelse", "Waiting for confirmation"),
        _ => Lang.T("Händelse", "Event"),
    };

    /// <summary>Glyph for a transfer direction: "down" or "up".</summary>
    public static string DirectionGlyph(string? direction) => direction == "up" ? "" : "";

    /// <summary>The word for a transfer direction, for screen readers.</summary>
    public static string DirectionLabel(string? direction) =>
        direction == "up" ? Lang.T("Laddar upp", "Uploading") : Lang.T("Hämtar", "Downloading");

    /// <summary>Short extension label for a file-type badge. Empty for event labels and folders.</summary>
    public static string FileTypeLabel(string? name)
    {
        if (string.IsNullOrWhiteSpace(name)) return string.Empty;
        var extension = System.IO.Path.GetExtension(name).TrimStart('.');
        if (extension.Length is < 1 or > 5 || !extension.All(char.IsLetterOrDigit)) return string.Empty;
        return extension.ToUpperInvariant() switch
        {
            "JPEG" => "JPG",
            "TIFF" => "TIF",
            var label when label.Length <= 4 => label,
            _ => "FILE",
        };
    }

    /// <summary>Segoe Fluent file-type glyph, chosen from the name alone so a cloud file stays unopened.</summary>
    public static string FileTypeGlyph(string? name) => FileTypeCategory(name) switch
    {
        "pdf" => "\uEA90",      // PDF
        "image" => "\uE8B9",    // Picture
        "document" => "\uE8A5", // Document
        "spreadsheet" => "\uF0E2", // GridView
        "design" => "\uEB3C",   // Design
        "archive" => "\uF012",  // ZipFolder
        _ => "\uE7C3",          // Page
    };

    /// <summary>Readable file-type name for the icon's tooltip and accessibility label.</summary>
    public static string FileTypeDescription(string? name) => FileTypeCategory(name) switch
    {
        "pdf" => Lang.T("PDF-dokument", "PDF document"),
        "image" => Lang.T("Bildfil", "Image file"),
        "document" => Lang.T("Dokument", "Document"),
        "spreadsheet" => Lang.T("Kalkylblad", "Spreadsheet"),
        "design" => Lang.T("CAD- eller BIM-fil", "CAD or BIM file"),
        "archive" => Lang.T("Arkivfil", "Archive file"),
        _ => Lang.T("Fil", "File"),
    };

    private static string FileTypeCategory(string? name)
    {
        var extension = System.IO.Path.GetExtension(name ?? string.Empty).ToLowerInvariant();
        return extension switch
        {
            ".pdf" => "pdf",
            ".jpg" or ".jpeg" or ".png" or ".gif" or ".bmp" or ".tif" or ".tiff" or
                ".webp" or ".heic" or ".svg" => "image",
            ".doc" or ".docx" or ".docm" or ".odt" or ".rtf" => "document",
            ".xls" or ".xlsx" or ".xlsm" or ".xlsb" or ".ods" or ".csv" => "spreadsheet",
            ".dwg" or ".dxf" or ".dwf" or ".dwfx" or ".rvt" or ".rfa" or ".rte" or
                ".ifc" or ".nwc" or ".nwd" or ".skp" or ".rce" or ".rcp" or ".rcs" => "design",
            ".zip" or ".7z" or ".rar" or ".tar" or ".gz" or ".tgz" => "archive",
            _ => "file",
        };
    }

    private static bool TryLocal(long tsMs, out DateTimeOffset when)
    {
        // FromUnixTimeMilliseconds throws outside year 1–9999. A bad timestamp must
        // not take a binding (and with it the window) down.
        if (tsMs <= 0 || tsMs > MaxUnixMs)
        {
            when = default;
            return false;
        }

        when = DateTimeOffset.FromUnixTimeMilliseconds(tsMs).ToLocalTime();
        return true;
    }

    private static (double Value, int Unit) Split(long bytes)
    {
        double value = Math.Max(0, bytes);
        var unit = 0;
        while (value >= 1024 && unit < Units.Length - 1)
        {
            value /= 1024;
            unit++;
        }

        return (value, unit);
    }

    private static string Num(double value, int unit)
    {
        if (unit == 0)
        {
            return value.ToString("0", Culture);
        }

        return value >= 100 ? value.ToString("0", Culture) : value.ToString("0.0", Culture);
    }
}

/// <summary>
/// Brings a bound collection to the wanted order with few changes, so a list keeps
/// its rows, its focus and its scroll position when one item arrives or leaves.
/// Items are compared by reference. Moves are a remove and an insert: the XAML
/// list controls handle those two, and nothing else is needed.
/// </summary>
internal static class ListSync
{
    public static void Apply<T>(ObservableCollection<T> view, IReadOnlyList<T> desired)
        where T : class
    {
        for (var i = 0; i < desired.Count; i++)
        {
            if (i < view.Count && ReferenceEquals(view[i], desired[i]))
            {
                continue;
            }

            for (var k = i + 1; k < view.Count; k++)
            {
                if (ReferenceEquals(view[k], desired[i]))
                {
                    view.RemoveAt(k);
                    break;
                }
            }

            view.Insert(i, desired[i]);
        }

        while (view.Count > desired.Count)
        {
            view.RemoveAt(view.Count - 1);
        }
    }
}

/// <summary>long (bytes) to "12,3 MB".</summary>
public sealed class BytesConverter : IValueConverter
{
    public object Convert(object value, Type targetType, object parameter, string language)
    {
        return value is long bytes ? Fmt.Bytes(bytes) : Fmt.Bytes(null);
    }

    public object ConvertBack(object value, Type targetType, object parameter, string language)
    {
        throw new NotSupportedException();
    }
}

/// <summary>long (Unix milliseconds) to "för 3 min sedan".</summary>
public sealed class RelativeTimeConverter : IValueConverter
{
    public object Convert(object value, Type targetType, object parameter, string language)
    {
        return value is long ms ? Fmt.Ago(ms) : string.Empty;
    }

    public object ConvertBack(object value, Type targetType, object parameter, string language)
    {
        throw new NotSupportedException();
    }
}

/// <summary>Activity kind string to its Segoe Fluent glyph.</summary>
public sealed class KindGlyphConverter : IValueConverter
{
    public object Convert(object value, Type targetType, object parameter, string language)
    {
        return Fmt.KindGlyph(value as string);
    }

    public object ConvertBack(object value, Type targetType, object parameter, string language)
    {
        throw new NotSupportedException();
    }
}

/// <summary>bool to Visibility. Set Invert="True" for the opposite.</summary>
public sealed class BoolToVisibilityConverter : IValueConverter
{
    public bool Invert { get; set; }

    public object Convert(object value, Type targetType, object parameter, string language)
    {
        var on = value is bool b && b;
        return on != Invert ? Visibility.Visible : Visibility.Collapsed;
    }

    public object ConvertBack(object value, Type targetType, object parameter, string language)
    {
        throw new NotSupportedException();
    }
}
