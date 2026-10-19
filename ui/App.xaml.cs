using System;
using System.Threading.Tasks;
using Synka.Services;
using Microsoft.UI.Xaml;
using Microsoft.Windows.AppLifecycle;

namespace Synka;

public partial class App : Application
{
    /// <summary>The key the one window registers under. A second start finds it and hands over.</summary>
    private const string InstanceKey = "Synka.Window";

    /// <summary>
    /// The main window. The file and folder pickers in WinUI 3 need a window
    /// handle, so it must be reachable from the pages.
    /// </summary>
    public static Window Window { get; private set; } = null!;

    public App()
    {
        InitializeComponent();

        // Without this a crash leaves only a stowed exception in the event log.
        // With it the whole chain ends up in ui-diagnostik.log. The window is a view
        // of the engine's state and holds nothing that could be lost, so it stays
        // open: a failed button handler must not take the whole window down.
        UnhandledException += (_, args) =>
        {
            DiagnosticLog.Write("Application.UnhandledException", args.Exception);
            args.Handled = true;
        };
        AppDomain.CurrentDomain.UnhandledException += (_, args) =>
        {
            DiagnosticLog.Write("AppDomain.UnhandledException", args.ExceptionObject as Exception);
        };
        // An async void handler whose exception nobody receives shows only here.
        TaskScheduler.UnobservedTaskException += (_, args) =>
        {
            DiagnosticLog.Write("TaskScheduler.UnobservedTaskException", args.Exception);
            args.SetObserved();
        };
    }

    protected override async void OnLaunched(LaunchActivatedEventArgs args)
    {
        // One window. The tray icon starts Synka.exe on every click; a second
        // start hands its activation to the window that is already open and quits.
        if (await HandedToRunningWindowAsync())
        {
            Exit();
            return;
        }

        // The window always shows, whatever the engine does. Connecting happens
        // in the background and reports itself in the window.
        Window = new MainWindow();
        Window.Activate();

        // The first-run notice comes before anything else. Until it is accepted the
        // engine is not started, so nothing reaches Autodesk for a user who quits here.
        var main = (MainWindow)Window;
        if (!await main.EnsureNoticeAcceptedAsync())
        {
            Exit();
            return;
        }

        _ = EngineClient.Instance.StartAsync();
        main.OfferDesktopShortcutIfNeeded();
    }

    /// <summary>
    /// True when another Synka window is open and has been handed this start.
    /// Anything that goes wrong here means "no": a second window is better than none.
    /// </summary>
    private static async Task<bool> HandedToRunningWindowAsync()
    {
        try
        {
            var main = AppInstance.FindOrRegisterForKey(InstanceKey);
            if (main.IsCurrent)
            {
                main.Activated += OnActivationHandedOver;
                return false;
            }

            // Handed over off the UI thread, and not waited on for ever: a window that
            // does not answer must not leave this start hanging without a window.
            var activation = AppInstance.GetCurrent().GetActivatedEventArgs();
            await Task.Run(() => main.RedirectActivationToAsync(activation).AsTask())
                .WaitAsync(TimeSpan.FromSeconds(5));
            return true;
        }
        catch (Exception exception)
        {
            DiagnosticLog.Write("en enda instans", exception);
            return false;
        }
    }

    /// <summary>A second start was handed over. Raised on a background thread.</summary>
    private static void OnActivationHandedOver(object? sender, AppActivationArguments args)
    {
        if (Window is MainWindow window)
        {
            window.DispatcherQueue.TryEnqueue(window.BringToFront);
        }
    }
}
