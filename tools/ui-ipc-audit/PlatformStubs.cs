// This executable tests the production protocol client without starting WinUI or the engine.
// Only platform services unrelated to pipe I/O are replaced. No app process is launched:
// tests invoke ServeAsync directly on a unique, private named pipe, never StartAsync.
namespace Microsoft.UI.Dispatching
{
    public sealed class DispatcherQueue
    {
        public bool HasThreadAccess => true;
        public static DispatcherQueue? GetForCurrentThread() => null;
        public bool TryEnqueue(Action action) { action(); return true; }
    }
}

namespace Windows.ApplicationModel
{
    public sealed class Package
    {
        public static Package Current => throw new InvalidOperationException("Unpackaged audit executable");
        public PackageId Id => new();
    }

    public sealed class PackageId
    {
        public string FamilyName => throw new InvalidOperationException("No package in protocol audit");
    }
}

namespace Synka.Services
{
    internal static class Lang
    {
        public static string T(string swedish, string english) => english;
    }

    internal static class DiagnosticLog
    {
        public static void Write(string area, Exception? exception, string[]? details = null) =>
            Console.Error.WriteLine($"CLIENT DIAGNOSTIC [{area}]: {exception?.GetType().Name ?? "message"}");
    }
}
