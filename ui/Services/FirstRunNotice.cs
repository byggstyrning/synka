using System;
using System.Globalization;
using System.IO;
using Windows.Storage;

namespace Synka.Services;

/// <summary>
/// Records that the user accepted the first-run notice (the legal terms: independent
/// product, Autodesk's terms still apply, the hub admin approval, ACC is the master
/// copy). The window shows it before the engine is started; see App.OnLaunched.
///
/// Bump <see cref="NoticeVersion"/> whenever the text changes materially: everyone is asked again.
/// </summary>
public static class FirstRunNotice
{
    /// <summary>The version of the notice text the user accepts.</summary>
    public const int NoticeVersion = 1;

    private const string VersionKey = "NoticeAcceptedVersion";

    private const string AcceptedAtKey = "NoticeAcceptedAtUtc";

    /// <summary>Unpackaged development runs have no LocalSettings. "&lt;version&gt; &lt;ISO-8601 UTC&gt;" lives here instead.</summary>
    private const string FallbackFile = "notice-accepted.txt";

    /// <summary>True when the current version of the notice has been accepted.</summary>
    public static bool IsAccepted()
    {
        try
        {
            return ApplicationData.Current.LocalSettings.Values[VersionKey] is int version && version >= NoticeVersion;
        }
        catch (Exception)
        {
            // Without package identity there is no LocalSettings. The file stands in for it.
            return FallbackVersion() >= NoticeVersion;
        }
    }

    /// <summary>Records the acceptance of the current version, with the time it was given.</summary>
    public static void RecordAcceptance()
    {
        var at = DateTime.UtcNow.ToString("o", CultureInfo.InvariantCulture);
        try
        {
            var values = ApplicationData.Current.LocalSettings.Values;
            values[VersionKey] = NoticeVersion;
            values[AcceptedAtKey] = at;
        }
        catch (Exception)
        {
            try
            {
                Directory.CreateDirectory(AppPaths.UserData);
                File.WriteAllText(Path.Combine(AppPaths.UserData, FallbackFile), $"{NoticeVersion} {at}");
            }
            catch (Exception exception)
            {
                // A notice that cannot be recorded is asked again next start. Nothing else is lost.
                DiagnosticLog.Write("villkor", exception);
            }
        }
    }

    /// <summary>The version in the fallback file, or 0 when there is none or it cannot be read.</summary>
    private static int FallbackVersion()
    {
        try
        {
            var text = File.ReadAllText(Path.Combine(AppPaths.UserData, FallbackFile));
            var first = text.Split(' ', StringSplitOptions.RemoveEmptyEntries);
            return first.Length > 0 && int.TryParse(first[0], NumberStyles.Integer, CultureInfo.InvariantCulture, out var version)
                ? version
                : 0;
        }
        catch (Exception)
        {
            return 0;
        }
    }
}
