using System;
using System.Collections.Generic;
using System.IO;
using System.Text;

namespace Synka.Services;

/// <summary>
/// The window's notepad: what it did at startup, and what went wrong when something did.
///
/// A WinUI app that falls over on an unhandled exception leaves only a stowed
/// exception in the event log: an HRESULT and a module, nothing about what went
/// wrong. This file is what makes a crash on someone's machine debuggable.
///
/// It is APPENDED to, and it starts over once it passes a size ceiling. The
/// logging may never throw itself: the app may already be on its way down.
/// </summary>
public static class DiagnosticLog
{
    /// <summary>The file's name. Lives in <see cref="AppPaths.UserData"/>, next to the engine's log file name, which is never the same.</summary>
    public const string FileName = "ui-diagnostik.log";

    /// <summary>How large the file may grow before it starts over. A guard against a crash loop, not a limit anyone reads.</summary>
    private const long MaxBytes = 512 * 1024;

    private static readonly UTF8Encoding Utf8 = new(false);

    private static string Folder => AppPaths.UserData;

    public static string File => Path.Combine(Folder, FileName);

    /// <param name="origin">Where the error was caught. Stands first in the entry.</param>
    /// <param name="exception">The exception, if there is one. The whole inner chain is written.</param>
    /// <param name="notes">Facts to write before the exception, such as the paths in use at startup.</param>
    public static void Write(string origin, Exception? exception, IEnumerable<string>? notes = null)
    {
        try
        {
            Directory.CreateDirectory(Folder);
            if (System.IO.File.Exists(File) && new FileInfo(File).Length > MaxBytes)
            {
                System.IO.File.Delete(File);
            }

            System.IO.File.AppendAllText(File, Entry(origin, exception, notes), Utf8);
        }
        catch (Exception)
        {
            // A log that cannot be written is bad enough. Crashing on it would be worse.
        }
    }

    private static string Entry(string origin, Exception? exception, IEnumerable<string>? notes)
    {
        var text = new StringBuilder()
            .AppendLine()
            .AppendLine("================================================================")
            .Append(DateTime.Now.ToString("yyyy-MM-dd HH:mm:ss"))
            .Append("  ")
            .AppendLine(origin)
            .AppendLine("================================================================");

        foreach (var note in notes ?? Array.Empty<string>())
        {
            text.AppendLine(note);
        }

        for (var current = exception; current is not null; current = current.InnerException)
        {
            text.AppendLine()
                .Append(current.GetType().FullName)
                .Append(": ")
                .AppendLine(current.Message)
                .AppendLine(current.StackTrace ?? "  (no stack)");
            if (current is System.Runtime.InteropServices.COMException com)
            {
                text.AppendLine($"  HRESULT 0x{com.HResult:X8}");
            }
        }

        return text.ToString();
    }
}
