using System;
using System.IO;
using Windows.Storage;

namespace Synka.Services;

/// <summary>
/// The window's own folder: the package's LocalState, where the window keeps its
/// notepad (the diagnostic log) and its saved choices.
///
/// The folder is PHYSICAL (<c>ApplicationData.Current.LocalFolder.Path</c>), not a
/// path assembled from %LOCALAPPDATA%. Inside the package a write there is
/// redirected into the container, and the engine does not see that redirection
/// (the same lesson as in Koordinatverktyget). The window therefore asks for the
/// folder rather than guessing it.
///
/// The engine's own folders (sync root, logs) are NOT here: the engine sends
/// its paths in `status` and `settings`, and the window shows them as they are.
/// </summary>
public static class AppPaths
{
    private const string AppName = "Synka";

    /// <summary>The window's folder. Not created here — the writer creates it.</summary>
    public static string UserData => _resolved ??= Resolve();

    private static string? _resolved;

    private static string Resolve()
    {
        try
        {
            return ApplicationData.Current.LocalFolder.Path;
        }
        catch (Exception)
        {
            // Without package identity there is no LocalFolder. Development runs
            // then use %LOCALAPPDATA%, where there is no redirection to take into account.
            return Path.Combine(
                Environment.GetEnvironmentVariable("LOCALAPPDATA")
                    ?? Environment.GetFolderPath(Environment.SpecialFolder.UserProfile),
                AppName);
        }
    }
}
