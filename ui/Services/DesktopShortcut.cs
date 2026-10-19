using System;
using System.IO;
using System.Runtime.InteropServices;
using System.Runtime.InteropServices.ComTypes;
using System.Text;

namespace Synka.Services;

/// <summary>
/// The desktop shortcut to the window, and the one-time question about it.
///
/// The shortcut points at the AUMID (shell:AppsFolder\&lt;family&gt;!App), not at
/// the exe path: the exe moves on every update while the family name stays. The
/// target is set as an ID list via IShellLinkW, because WScript.Shell accepts only
/// file paths as TargetPath.
/// </summary>
public static class DesktopShortcut
{
    private const string PromptedKey = "DesktopShortcutPrompted";

    private const string LinkFileName = "Synka.lnk";

    /// <summary>True when the question has never been asked. Without package identity it is never asked.</summary>
    public static bool ShouldOffer()
    {
        try
        {
            return !Windows.Storage.ApplicationData.Current.LocalSettings.Values.ContainsKey(PromptedKey);
        }
        catch (Exception)
        {
            return false;
        }
    }

    /// <summary>
    /// Marks the question as asked, whatever the answer. Done before the dialog
    /// is shown: a crash or a closed window must not make the question come back.
    /// </summary>
    public static void MarkOffered()
    {
        try
        {
            Windows.Storage.ApplicationData.Current.LocalSettings.Values[PromptedKey] = true;
        }
        catch (Exception)
        {
            // Without LocalSettings the question is never asked anyway.
        }
    }

    /// <summary>
    /// Creates the shortcut on the desktop. Null means done (also when a shortcut
    /// already exists — a user's own is never overwritten). Otherwise a message to
    /// show. Never throws.
    /// </summary>
    public static string? TryCreate()
    {
        try
        {
            // Known folders are not redirected by the package container, so this is the real desktop.
            var desktop = Environment.GetFolderPath(Environment.SpecialFolder.Desktop);
            var linkPath = Path.Combine(desktop, LinkFileName);
            if (File.Exists(linkPath))
            {
                return null;
            }

            var aumid = $"{Windows.ApplicationModel.Package.Current.Id.FamilyName}!App";
            Create(linkPath, aumid, Environment.ProcessPath);
            return null;
        }
        catch (Exception exception)
        {
            return Lang.T("Genvägen gick inte att skapa: ", "The shortcut could not be created: ") + exception.Message;
        }
    }

    /// <summary>Writes a .lnk whose target is an AUMID.</summary>
    internal static void Create(string linkPath, string aumid, string? iconPath)
    {
        var link = (IShellLinkW)new ShellLink();

        // shell:AppsFolder resolves the AUMID to the same entry the Start menu uses.
        Marshal.ThrowExceptionForHR(SHParseDisplayName(
            $"shell:AppsFolder\\{aumid}", IntPtr.Zero, out var idList, 0, out _));
        try
        {
            link.SetIDList(idList);
        }
        finally
        {
            Marshal.FreeCoTaskMem(idList);
        }

        if (iconPath is not null && File.Exists(iconPath))
        {
            link.SetIconLocation(iconPath, 0);
        }

        link.SetDescription("Synka");
        ((IPersistFile)link).Save(linkPath, true);
    }

    [DllImport("shell32.dll", CharSet = CharSet.Unicode)]
    private static extern int SHParseDisplayName(
        string name, IntPtr bindingContext, out IntPtr idList, uint sfgaoIn, out uint sfgaoOut);

    [ComImport]
    [Guid("00021401-0000-0000-C000-000000000046")]
    private class ShellLink
    {
    }

    /// <summary>The whole vtable must be declared in order, unused methods included: COM calls by position.</summary>
    [ComImport]
    [Guid("000214F9-0000-0000-C000-000000000046")]
    [InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
    private interface IShellLinkW
    {
        void GetPath([Out, MarshalAs(UnmanagedType.LPWStr)] StringBuilder file, int maxPath, IntPtr findData, uint flags);
        void GetIDList(out IntPtr idList);
        void SetIDList(IntPtr idList);
        void GetDescription([Out, MarshalAs(UnmanagedType.LPWStr)] StringBuilder description, int maxLength);
        void SetDescription([MarshalAs(UnmanagedType.LPWStr)] string description);
        void GetWorkingDirectory([Out, MarshalAs(UnmanagedType.LPWStr)] StringBuilder directory, int maxLength);
        void SetWorkingDirectory([MarshalAs(UnmanagedType.LPWStr)] string directory);
        void GetArguments([Out, MarshalAs(UnmanagedType.LPWStr)] StringBuilder arguments, int maxLength);
        void SetArguments([MarshalAs(UnmanagedType.LPWStr)] string arguments);
        void GetHotkey(out ushort hotkey);
        void SetHotkey(ushort hotkey);
        void GetShowCmd(out int showCmd);
        void SetShowCmd(int showCmd);
        void GetIconLocation([Out, MarshalAs(UnmanagedType.LPWStr)] StringBuilder iconPath, int maxLength, out int iconIndex);
        void SetIconLocation([MarshalAs(UnmanagedType.LPWStr)] string iconPath, int iconIndex);
        void SetRelativePath([MarshalAs(UnmanagedType.LPWStr)] string relativePath, uint reserved);
        void Resolve(IntPtr window, uint flags);
        void SetPath([MarshalAs(UnmanagedType.LPWStr)] string file);
    }
}
