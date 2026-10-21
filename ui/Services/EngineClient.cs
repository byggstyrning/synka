using System;
using System.Collections.Generic;
using System.Collections.ObjectModel;
using System.ComponentModel;
using System.Diagnostics;
using System.IO;
using System.IO.Pipes;
using System.Reflection;
using System.Security.Principal;
using System.Text;
using System.Text.Json;
using System.Text.Json.Nodes;
using System.Threading;
using System.Threading.Tasks;
using Microsoft.UI.Dispatching;
using Windows.ApplicationModel;

namespace Synka.Services;

/// <summary>Where the window stands with the engine. Shown in the InfoBar and the title bar.</summary>
public enum EngineConnectionState
{
    /// <summary>First attempts after the window opened; the engine may simply be starting up.</summary>
    Connecting,

    /// <summary>The engine has been asked to start; retries continue.</summary>
    Starting,

    /// <summary>Connected: messages flow both ways.</summary>
    Connected,

    /// <summary>
    /// The connection was lost; retries continue, but the window does not start the
    /// engine by itself: the tray's "Avsluta" closes the pipe too, and that is the
    /// user's choice. The InfoBar offers to start it.
    /// </summary>
    Disconnected,

    /// <summary>
    /// Nothing answers and nothing is being done about it: the user quit the engine,
    /// or a start did not bring it up. Retries continue quietly; the user can start it.
    /// </summary>
    NotRunning,

    /// <summary>
    /// The engine closed the pipe to restart itself after a sync root move. Retries
    /// continue; the window starts the engine only if it has not come back after a while.
    /// </summary>
    Restarting,
}

/// <summary>
/// The window's end of the pipe to the engine (docs\IPC.md).
///
/// One instance for the whole window. The engine is the single source of truth:
/// everything the pages show is a copy of the last message the engine sent, and
/// every change goes back as a command. The public state is updated and every
/// change notification is raised ON THE UI THREAD, so the pages can bind to it
/// directly.
/// </summary>
public sealed class EngineClient : INotifyPropertyChanged
{
    private static readonly Lazy<EngineClient> Shared = new(() => new EngineClient());

    /// <summary>The window's connection to the engine. Created on first use.</summary>
    public static EngineClient Instance => Shared.Value;

    /// <summary>The window's version, as the engine is told on `hello`.</summary>
    public static string AppVersion =>
        Assembly.GetExecutingAssembly().GetName().Version is { } version
            ? $"{version.Major}.{version.Minor}.{version.Build}"
            : "0.0.0";

    /// <summary>
    /// How long one connection attempt may wait. When the pipe does not exist,
    /// NamedPipeClientStream spins for the whole timeout, so it is kept short and
    /// the pause between attempts does the waiting.
    /// </summary>
    private const int ConnectTimeoutMs = 250;

    /// <summary>The pause between attempts (docs\IPC.md: "retries every 1 s").</summary>
    private const int RetryDelayMs = 1000;

    /// <summary>After a long outage the attempts slow down: the engine is not coming by itself.</summary>
    private const int SlowRetryDelayMs = 3000;

    private static readonly TimeSpan SlowRetryAfter = TimeSpan.FromSeconds(30);

    /// <summary>
    /// When the window opens and nothing answers for this long, the engine is
    /// started (docs\IPC.md: "retries every 1 s for 10 s. If the engine isn't
    /// running, it starts it"). Measured in time, not in attempts.
    /// </summary>
    private static readonly TimeSpan AutoStartAfter = TimeSpan.FromSeconds(10);

    /// <summary>How long a start may take before the window gives up and says so.</summary>
    private const long StartGraceMs = 30_000;

    /// <summary>
    /// While a start is pending the engine is launched again this often. A second
    /// engine exits at once (single-instance mutex), so this is harmless, and it
    /// covers a restart where the old engine still held the mutex at the first launch.
    /// </summary>
    private const long RelaunchEveryMs = 5_000;

    /// <summary>
    /// After a sync root move the engine starts itself again (the new process waits up
    /// to 30 s for the old one to exit). The window waits this long before it starts
    /// the engine itself, so the two starts do not race.
    /// </summary>
    private const long RestartGraceMs = 20_000;

    /// <summary>
    /// A pipe that closes this soon after a move was asked for is the engine's restart,
    /// even when no `status` has yet said the move is under way.
    /// </summary>
    private const long MoveAnswerMs = 15_000;

    /// <summary>The `status` text while the engine moves the sync root (docs\IPC.md, move_sync_root).</summary>
    private const string MovingText = "Flyttar synkmappen";

    private const int ActivityLimit = 200;

    /// <summary>
    /// How long a command waits for its ack. The engine acks a command as soon as
    /// it has accepted it (docs\IPC.md, "Ack"); the work itself is reported later
    /// as events. A timeout is reported as "did not answer", never as a failure of the work.
    /// </summary>
    private static readonly TimeSpan CommandTimeout = TimeSpan.FromSeconds(15);

    private const int MaxPendingCommands = 128;
    private const int MaxCommandBytes = 64 * 1024;
    private const int MaxEventChars = 16 * 1024 * 1024;

    /// <summary>Fallback if the engine accepts free-up but never sends its completion notification.</summary>
    private static readonly TimeSpan FreeUpAnswerTimeout = TimeSpan.FromMinutes(10);

    private static readonly UTF8Encoding Utf8 = new(false);

    private static readonly Lazy<string> PipeNameValue = new(() =>
        "synka-ui-" + WindowsIdentity.GetCurrent().User!.Value);

    private readonly TaskCompletionSource _firstAttempt =
        new(TaskCreationOptions.RunContinuationsAsynchronously);

    /// <summary>Serialises writes: one line at a time on the pipe.</summary>
    private readonly SemaphoreSlim _sendLock = new(1, 1);

    /// <summary>Guards <see cref="_writer"/> and <see cref="_pending"/>.</summary>
    private readonly object _lock = new();

    /// <summary>Serializes free-up requests made from different pages in this window.</summary>
    private readonly object _freeUpGate = new();

    private readonly Dictionary<long, TaskCompletionSource<Ack>> _pending = new();

    private long _nextId;
    private StreamWriter? _writer;
    private DispatcherQueue? _dispatcher;
    private Task? _supervisor;

    /// <summary>Set when the user has asked the engine to quit: no more starting it.</summary>
    private volatile bool _quitByUser;

    /// <summary>Set by <see cref="RestartEngineAsync"/>: start the engine again once it has gone.</summary>
    private volatile bool _restartWhenGone;

    /// <summary>Environment.TickCount64 when a start was requested; 0 when none is pending.</summary>
    private long _startRequestedAt;

    /// <summary>Environment.TickCount64 of the last launch of the engine process.</summary>
    private long _lastLaunchAt;

    /// <summary>Environment.TickCount64 when a sync root move was asked for; 0 when none is.</summary>
    private long _moveRequestedAt;

    /// <summary>The engine's last `status` said it is moving the sync root.</summary>
    private volatile bool _statusSaysMoving;

    /// <summary>Environment.TickCount64 when the pipe closed for a restart after a move; 0 otherwise.</summary>
    private long _restartingSince;

    private bool _isConnected;
    private EngineConnectionState _connectionState = EngineConnectionState.Connecting;
    private StatusInfo? _status;
    private ProjectsInfo? _projects;
    private SettingsInfo? _settings;
    private UpdateInfo? _update;
    private StreamBimRegionsInfo? _streamBimRegions;

    /// <summary>
    /// The latest sign_in_progress per provider and StreamBIM account key (null for the other
    /// providers), UI thread. Kept so a page that opens mid-sign-in can catch up.
    /// </summary>
    private readonly Dictionary<(string Provider, string? Account), SignInProgressInfo> _signInProgress = new();

    private bool _isFreeingUp;
    private int _freeUpGeneration;

    private EngineClient()
    {
    }

    public event PropertyChangedEventHandler? PropertyChanged;

    /// <summary>Raised for every `notify` message from the engine.</summary>
    public event EventHandler<NotifyInfo>? Notified;

    /// <summary>
    /// Raised for every `sign_in_progress` message (on the UI thread). The state is also
    /// kept per provider, see <see cref="SignInProgressFor"/>.
    /// </summary>
    public event Action<SignInProgressInfo>? SignInProgress;

    /// <summary>Raised when the engine sends the StreamBIM region list (on the UI thread).</summary>
    public event Action<StreamBimRegionsInfo>? StreamBimRegionsChanged;

    internal ApsClientIdEditState ApsClientIdEdit { get; } = new();

    public ApsClientIdValidationInfo? ApsClientIdValidation { get; private set; }

    /// <summary>True while the pipe is open and the engine is answering.</summary>
    public bool IsConnected => _isConnected;

    /// <summary>Finer than <see cref="IsConnected"/>: whether the engine is being started.</summary>
    public EngineConnectionState ConnectionState => _connectionState;

    /// <summary>The engine's last status message.</summary>
    public StatusInfo? Status => _status;

    /// <summary>
    /// Connected, and at least one provider has an account (docs\IPC.md: `signed_in` is true
    /// when any provider is signed in). The commands that need an account wait for this.
    /// </summary>
    public bool IsSignedIn => _isConnected && _status is { SignedIn: true };

    /// <summary>
    /// Every provider's account as the engine last reported it (`acc`, `streambim`,
    /// `interaxo`). Empty before the first status, and from an engine without `accounts`.
    /// </summary>
    public IReadOnlyList<AccountInfo> Accounts => _status?.Accounts ?? Array.Empty<AccountInfo>();

    /// <summary>
    /// The account of one provider, or null when the engine does not report it. An engine
    /// without `accounts` (older) reports ACC only, from `signed_in` and `user`.
    /// </summary>
    public AccountInfo? AccountFor(string provider)
    {
        var status = _status;
        if (status is null)
        {
            return null;
        }

        if (status.Accounts is { } accounts)
        {
            foreach (var account in accounts)
            {
                if (account is not null && string.Equals(account.Provider, provider, StringComparison.Ordinal))
                {
                    return account;
                }
            }
            return null;
        }

        return provider == "acc"
            ? new AccountInfo("acc", "ACC", true, status.SignedIn, false, status.User, null)
            : null;
    }

    /// <summary>
    /// True when the engine offers this provider: `settings.providers` names it. Interaxo is left out
    /// while it is turned off (settings.json `interaxo_enabled`). An engine that does not send
    /// `providers` (older) offers every provider, as it always did.
    /// </summary>
    public bool ProviderOffered(string provider)
    {
        var providers = _settings?.Providers;
        if (providers is null)
        {
            return true;
        }

        foreach (var offered in providers)
        {
            if (string.Equals(offered, provider, StringComparison.Ordinal))
            {
                return true;
            }
        }
        return false;
    }

    /// <summary>
    /// The StreamBIM accounts the engine reports (one per server that is signed in or signing
    /// in), in the engine's order. Empty before the first status and from an older engine.
    /// </summary>
    public IReadOnlyList<AccountInfo> StreamBimAccounts
    {
        get
        {
            var list = new List<AccountInfo>();
            foreach (var account in Accounts)
            {
                if (account is not null && string.Equals(account.Provider, "streambim", StringComparison.Ordinal))
                {
                    list.Add(account);
                }
            }
            return list;
        }
    }

    /// <summary>
    /// The latest sign_in_progress for a provider and, for StreamBIM, its account key; null when
    /// none has come since the window started. ACC and Interaxo have no account (pass none).
    /// </summary>
    public SignInProgressInfo? SignInProgressFor(string provider, string? account = null) =>
        _signInProgress.TryGetValue((provider, NormalizeAccount(account)), out var progress) ? progress : null;

    /// <summary>An empty or blank account key is no account.</summary>
    private static string? NormalizeAccount(string? account) =>
        string.IsNullOrWhiteSpace(account) ? null : account;

    /// <summary>The StreamBIM regions the engine last sent, or null before the first list.</summary>
    public StreamBimRegionsInfo? StreamBimRegions => _streamBimRegions;

    /// <summary>True until this window's free-up request finishes or its answer expires.</summary>
    public bool IsFreeingUp
    {
        get
        {
            lock (_freeUpGate)
            {
                return _isFreeingUp;
            }
        }
    }

    /// <summary>Connected, and the engine is moving the sync root (it restarts when done).</summary>
    public bool IsMovingSyncRoot => _isConnected && IsMovingText(_status?.Text);

    /// <summary>The engine's last projects message (the hub tree).</summary>
    public ProjectsInfo? Projects => _projects;

    /// <summary>The engine's last settings message.</summary>
    public SettingsInfo? Settings => _settings;

    /// <summary>Newest first, at most 200 items.</summary>
    public ObservableCollection<ActivityItem> Activity { get; } = new();

    /// <summary>The result of the last update check.</summary>
    public UpdateInfo? Update => _update;

    /// <summary>
    /// Connects to the engine and keeps connecting. Call once, from the UI thread,
    /// after the window exists. Returns when the first attempt has ended: either
    /// connected, or the engine has been asked to start (retries then continue in
    /// the background).
    /// </summary>
    public async Task StartAsync()
    {
        if (_supervisor is null)
        {
            _dispatcher = DispatcherQueue.GetForCurrentThread();
            _supervisor = Task.Run(SuperviseAsync);
        }

        await _firstAttempt.Task;
    }

    // ------------------------------------------------------------ connection

    /// <summary>
    /// The connection loop: connect, serve until the pipe closes, connect again.
    /// Runs for the life of the window, on the thread pool. Nothing may escape
    /// it: an exception here would end the window's contact with the engine for good.
    /// </summary>
    private async Task SuperviseAsync()
    {
        var everConnected = false;
        var autoStarted = false;
        var outage = Stopwatch.StartNew();

        while (true)
        {
            var delay = RetryDelayMs;
            try
            {
                var pipe = await TryConnectAsync().ConfigureAwait(false);
                if (pipe is not null)
                {
                    everConnected = true;
                    _quitByUser = false;
                    Interlocked.Exchange(ref _startRequestedAt, 0);
                    Interlocked.Exchange(ref _restartingSince, 0);
                    _firstAttempt.TrySetResult();

                    try
                    {
                        await ServeAsync(pipe).ConfigureAwait(false);
                    }
                    finally
                    {
                        // The pipe closed: the engine quit or crashed, or restarts after a move.
                        outage.Restart();
                        if (_restartWhenGone)
                        {
                            _restartWhenGone = false;
                            _quitByUser = false;
                            RequestStart();
                        }
                        else if (TakeRestartExpected())
                        {
                            Interlocked.Exchange(ref _restartingSince, Environment.TickCount64);
                        }

                        PublishState(CurrentOutageState(everConnected, autoStarted), connected: false);
                    }
                }
                else
                {
                    // Opening the window is the one moment the window starts the engine
                    // by itself (docs\IPC.md, Transport). After a lost connection it
                    // does not: the tray's "Avsluta" closes the pipe as well.
                    if (!everConnected && !autoStarted && !_quitByUser
                        && Interlocked.Read(ref _startRequestedAt) == 0
                        && outage.Elapsed >= AutoStartAfter)
                    {
                        autoStarted = true;
                        RequestStart();
                    }

                    StartIfRestartIsLate();
                    RelaunchIfStarting();

                    var state = CurrentOutageState(everConnected, autoStarted);
                    PublishState(state, connected: false);
                    if (state != EngineConnectionState.Connecting)
                    {
                        _firstAttempt.TrySetResult();
                    }

                    if (outage.Elapsed >= SlowRetryAfter && state != EngineConnectionState.Starting)
                    {
                        delay = SlowRetryDelayMs;
                    }
                }
            }
            catch (Exception exception)
            {
                DiagnosticLog.Write("anslutningen till motorn", exception);
            }

            await Task.Delay(delay).ConfigureAwait(false);
        }
    }

    /// <summary>One attempt to open the pipe. Null when nothing answered.</summary>
    private static async Task<NamedPipeClientStream?> TryConnectAsync()
    {
        var pipe = new NamedPipeClientStream(".", PipeNameValue.Value, PipeDirection.InOut, PipeOptions.Asynchronous);
        try
        {
            await pipe.ConnectAsync(ConnectTimeoutMs).ConfigureAwait(false);
            return pipe;
        }
        catch (Exception)
        {
            // Timeout, or the pipe does not exist yet. Ordinary while the engine starts.
            pipe.Dispose();
            return null;
        }
    }

    /// <summary>What to show while there is no connection.</summary>
    private EngineConnectionState CurrentOutageState(bool everConnected, bool autoStarted)
    {
        if (Interlocked.Read(ref _restartingSince) != 0)
        {
            return EngineConnectionState.Restarting;
        }

        var requested = Interlocked.Read(ref _startRequestedAt);
        if (requested != 0)
        {
            if (Environment.TickCount64 - requested < StartGraceMs)
            {
                return EngineConnectionState.Starting;
            }

            // The start did not bring the engine up. Say so instead of spinning forever.
            Interlocked.CompareExchange(ref _startRequestedAt, 0, requested);
            return EngineConnectionState.NotRunning;
        }

        if (_quitByUser)
        {
            return EngineConnectionState.NotRunning;
        }

        if (everConnected)
        {
            return EngineConnectionState.Disconnected;
        }

        // Never connected: still in the first ten seconds, or the automatic start did not help.
        return autoStarted ? EngineConnectionState.NotRunning : EngineConnectionState.Connecting;
    }

    /// <summary>Marks a start as pending and launches the engine. Called on the thread pool.</summary>
    private void RequestStart()
    {
        var now = Environment.TickCount64;
        Interlocked.Exchange(ref _startRequestedAt, now);
        Interlocked.Exchange(ref _lastLaunchAt, now);
        StartEngineProcess();
    }

    /// <summary>While a start is pending, launches the engine again every few seconds.</summary>
    private void RelaunchIfStarting()
    {
        if (Interlocked.Read(ref _startRequestedAt) == 0)
        {
            return;
        }

        var now = Environment.TickCount64;
        if (now - Interlocked.Read(ref _lastLaunchAt) >= RelaunchEveryMs)
        {
            Interlocked.Exchange(ref _lastLaunchAt, now);
            StartEngineProcess();
        }
    }

    /// <summary>
    /// True when the pipe that just closed was the engine restarting after a sync root
    /// move: its last `status` said so, or the move was asked for a moment ago. Called
    /// once per closed pipe; the expectation is used up either way.
    /// </summary>
    private bool TakeRestartExpected()
    {
        var requested = Interlocked.Exchange(ref _moveRequestedAt, 0);
        var moving = _statusSaysMoving;
        _statusSaysMoving = false;
        return moving || (requested != 0 && Environment.TickCount64 - requested < MoveAnswerMs);
    }

    /// <summary>
    /// After a move the engine starts itself again. Should it not be back after
    /// <see cref="RestartGraceMs"/>, the window starts it, as on a first start.
    /// </summary>
    private void StartIfRestartIsLate()
    {
        var since = Interlocked.Read(ref _restartingSince);
        if (since != 0
            && Environment.TickCount64 - since >= RestartGraceMs
            && Interlocked.CompareExchange(ref _restartingSince, 0, since) == since)
        {
            RequestStart();
        }
    }

    private static bool IsMovingText(string? text) =>
        text is not null && text.StartsWith(MovingText, StringComparison.Ordinal);

    /// <summary>
    /// Serves one connection: says hello, then reads lines until the pipe closes.
    /// Every line is handled on its own, so one bad message cannot end the connection.
    /// </summary>
    private async Task ServeAsync(NamedPipeClientStream pipe)
    {
        using var reader = new StreamReader(pipe, Utf8, false, 1 << 16, leaveOpen: true);
        lock (_lock)
        {
            _writer = new StreamWriter(pipe, Utf8, 1 << 16, leaveOpen: true) { AutoFlush = true, NewLine = "\n" };
        }

        PublishState(EngineConnectionState.Connected, connected: true);
        _ = SendAsync("hello", new Dictionary<string, object?> { ["version"] = AppVersion });

        try
        {
            var buffer = new char[4096];
            var frame = new StringBuilder();
            int read;
            while ((read = await reader.ReadAsync(buffer.AsMemory()).ConfigureAwait(false)) != 0)
            {
                for (var index = 0; index < read; index++)
                {
                    if (buffer[index] != '\n')
                    {
                        if (frame.Length >= MaxEventChars)
                        {
                            throw new InvalidDataException("Engine message exceeded the frame limit.");
                        }
                        frame.Append(buffer[index]);
                        continue;
                    }

                    var line = frame.ToString().TrimEnd('\r');
                    frame.Clear();
                    if (line.Length == 0) continue;
                    try
                    {
                        HandleLine(line);
                    }
                    catch (Exception exception)
                    {
                        DiagnosticLog.Write("meddelande från motorn", exception, new[] { line });
                    }
                }
            }
            // A disconnect before the newline leaves an incomplete frame, not an event.
        }
        catch (Exception exception) when (exception is IOException or ObjectDisposedException or InvalidOperationException)
        {
            // The pipe went away under the reader. Same as a clean close.
        }
        finally
        {
            lock (_lock)
            {
                _writer = null;
            }

            pipe.Dispose();
            FailPending(Lang.T("Anslutningen till motorn bröts.", "The connection to the engine was lost."));
        }
    }

    /// <summary>
    /// Sets the connection flags and state on the UI thread. Raises only what
    /// changed: the loop publishes after every attempt, about once a second.
    /// </summary>
    private void PublishState(EngineConnectionState state, bool connected)
    {
        Post(() =>
        {
            var signedInBefore = IsSignedIn;
            var changedConnection = _isConnected != connected;
            var changedState = _connectionState != state;
            _isConnected = connected;
            _connectionState = state;

            if (changedConnection)
            {
                // A sign-in progress belongs to the engine that reported it. After a disconnect
                // or a reconnect the engine's state is read again from its status, not from this cache.
                _signInProgress.Clear();
            }

            if (!connected)
            {
                ClearFreeUp();
            }

            if (changedConnection)
            {
                Raise(nameof(IsConnected));
            }

            if (signedInBefore != IsSignedIn)
            {
                Raise(nameof(IsSignedIn));
            }

            if (changedState || changedConnection)
            {
                Raise(nameof(ConnectionState));
            }
        });
    }

    // -------------------------------------------------------------- commands

    /// <summary>
    /// Sends one command and waits for its ack. Fields are written into the same
    /// JSON object as "cmd" and "id". Never throws: a command that cannot be sent
    /// comes back as an Ack with Ok false and an explanation in Error.
    /// </summary>
    public async Task<Ack> SendAsync(string cmd, object? fields = null)
    {
        var id = Interlocked.Increment(ref _nextId);
        lock (_lock)
        {
            if (_writer is null)
            {
                return new Ack(id, false, Lang.T("Ingen kontakt med motorn.", "No contact with the engine."));
            }
            if (_pending.Count >= MaxPendingCommands)
            {
                return new Ack(id, false, Lang.T("För många kommandon väntar. Försök igen strax.", "Too many commands are waiting. Try again shortly."));
            }
        }
        string line;
        try
        {
            line = BuildMessage(cmd, id, fields).ToJsonString();
            if (Utf8.GetByteCount(line) > MaxCommandBytes)
            {
                return new Ack(id, false, Lang.T("Kommandot är för stort.", "The command is too large."));
            }
        }
        catch (Exception exception)
        {
            return new Ack(id, false, exception.Message);
        }

        var completion = new TaskCompletionSource<Ack>(TaskCreationOptions.RunContinuationsAsynchronously);
        lock (_lock)
        {
            if (_writer is null || _pending.Count >= MaxPendingCommands)
            {
                return new Ack(id, false, Lang.T("Kommandot kunde inte köas. Försök igen.", "The command could not be queued. Try again."));
            }
            _pending[id] = completion;
        }

        // The timeout runs from before the write: a write that hangs (an engine that
        // has stopped reading) must not leave the caller waiting for ever.
        using var timeout = new CancellationTokenSource(CommandTimeout);
        using var registration = timeout.Token.Register(() =>
            Complete(id, new Ack(id, false, Lang.T("Motorn svarade inte.", "The engine did not answer."))));

        var written = WriteLineAsync(line, timeout.Token);
        try
        {
            if (await Task.WhenAny(written, completion.Task) == written && !await written)
            {
                Complete(id, new Ack(id, false, Lang.T("Ingen kontakt med motorn.", "No contact with the engine.")));
            }

            return await completion.Task;
        }
        finally
        {
            // A disconnected or expired caller must not leave an unsent command waiting
            // on the semaphore to execute later when the engine resumes reading.
            if (!written.IsCompleted) timeout.Cancel();
        }
    }

    private static JsonObject BuildMessage(string cmd, long id, object? fields)
    {
        var message = new JsonObject
        {
            ["cmd"] = cmd,
            ["id"] = id,
        };

        switch (fields)
        {
            case null:
                break;

            case IDictionary<string, object?> dictionary:
                foreach (var (key, value) in dictionary)
                {
                    message[key] = ToNode(value);
                }
                break;

            default:
                // Any other object: its properties become fields, named as the
                // serializer names them. Callers pass a dictionary for snake_case keys.
                if (JsonSerializer.SerializeToNode(fields, fields.GetType()) is JsonObject properties)
                {
                    foreach (var property in properties)
                    {
                        message[property.Key] = property.Value?.DeepClone();
                    }
                }
                break;
        }

        return message;
    }

    private static JsonNode? ToNode(object? value) =>
        value is null ? null : JsonSerializer.SerializeToNode(value, value.GetType());

    private async Task<bool> WriteLineAsync(string line, CancellationToken cancellationToken)
    {
        StreamWriter? writer;
        lock (_lock)
        {
            writer = _writer;
        }

        if (writer is null)
        {
            return false;
        }

        var acquired = false;
        try
        {
            await _sendLock.WaitAsync(cancellationToken).ConfigureAwait(false);
            acquired = true;
            cancellationToken.ThrowIfCancellationRequested();
            lock (_lock)
            {
                if (!ReferenceEquals(writer, _writer)) return false;
            }
            await writer.WriteLineAsync(line.AsMemory(), cancellationToken).ConfigureAwait(false);
            return true;
        }
        catch (OperationCanceledException)
        {
            // Canceling an active write may leave a partial JSON frame in the pipe.
            // Close that connection so the next command cannot complete its fragment.
            if (acquired)
            {
                try { writer.BaseStream.Dispose(); }
                catch (Exception exception) when (exception is IOException or ObjectDisposedException) { }
            }
            return false;
        }
        catch (Exception exception) when (exception is IOException or ObjectDisposedException or InvalidOperationException)
        {
            return false;
        }
        finally
        {
            if (acquired) _sendLock.Release();
        }
    }

    private void Complete(long id, Ack ack)
    {
        TaskCompletionSource<Ack>? completion;
        lock (_lock)
        {
            if (!_pending.Remove(id, out completion))
            {
                return;
            }
        }

        completion.TrySetResult(ack);
    }

    private void FailPending(string reason)
    {
        List<(long Id, TaskCompletionSource<Ack> Completion)> waiting;
        lock (_lock)
        {
            waiting = new List<(long, TaskCompletionSource<Ack>)>();
            foreach (var (id, completion) in _pending)
            {
                waiting.Add((id, completion));
            }
            _pending.Clear();
        }

        foreach (var (id, completion) in waiting)
        {
            completion.TrySetResult(new Ack(id, false, reason));
        }
    }

    // ------------------------------------------------------ incoming messages

    /// <summary>
    /// Reads one message. The message types are those of docs\IPC.md; a type the
    /// window does not know (a newer engine) is skipped, and so are fields it does
    /// not know. Each change of state is handed to the UI thread before it is applied.
    /// </summary>
    private void HandleLine(string line)
    {
        if (JsonNode.Parse(line) is not JsonObject root
            || root["type"] is not JsonValue typeValue
            || !typeValue.TryGetValue<string>(out var type))
        {
            return;
        }

        switch (type)
        {
            case "ack":
            {
                var ack = JsonSerializer.Deserialize(line, EngineJsonContext.Default.Ack);
                if (ack?.Id is { } ackId)
                {
                    Complete(ackId, ack with { IsEngineResponse = true });
                }
                else if (ack is { Ok: false })
                {
                    // A refusal the engine could not tie to a command (docs\IPC.md: "with
                    // "id" only when the request had one"). Every command here has an id,
                    // so this means a line the engine could not read at all.
                    DiagnosticLog.Write("motorn avvisade en rad", null, new[] { ack.Error ?? line });
                }
                break;
            }

            case "status":
            {
                var status = JsonSerializer.Deserialize(line, EngineJsonContext.Default.StatusInfo);
                if (status is not null)
                {
                    var moving = IsMovingText(status.Text);
                    if (_statusSaysMoving && !moving)
                    {
                        // The move was called off (a file was open): the engine stays.
                        Interlocked.Exchange(ref _moveRequestedAt, 0);
                    }
                    _statusSaysMoving = moving;

                    Post(() =>
                    {
                        var signedInBefore = IsSignedIn;
                        _status = status;
                        Raise(nameof(Status));
                        if (IsSignedIn != signedInBefore)
                        {
                            Raise(nameof(IsSignedIn));
                        }
                    });
                }
                break;
            }

            case "projects":
            {
                var projects = JsonSerializer.Deserialize(line, EngineJsonContext.Default.ProjectsInfo);
                if (projects is not null)
                {
                    Post(() =>
                    {
                        _projects = projects;
                        Raise(nameof(Projects));
                    });
                }
                break;
            }

            case "settings":
            {
                var settings = JsonSerializer.Deserialize(line, EngineJsonContext.Default.SettingsInfo);
                if (settings is not null)
                {
                    Post(() =>
                    {
                        _settings = settings;
                        if (settings.ApsClientIdOverrideSupported == true)
                            ApsClientIdEdit.ObserveSettings(settings.ApsClientId);
                        Raise(nameof(Settings));
                    });
                }
                break;
            }

            case "aps_client_id_validation":
            {
                var validation = JsonSerializer.Deserialize(line, EngineJsonContext.Default.ApsClientIdValidationInfo);
                if (validation is not null)
                {
                    Post(() =>
                    {
                        ApsClientIdValidation = validation;
                        ApsClientIdEdit.ObserveValidation(validation);
                        Raise(nameof(ApsClientIdValidation));
                    });
                }
                break;
            }

            case "sign_in_progress":
            {
                // docs\IPC.md: the progress of a sign-in. Nothing a password could be in: the
                // message carries only the provider, the state and the engine's Swedish error text.
                var received = JsonSerializer.Deserialize(line, EngineJsonContext.Default.SignInProgressInfo);
                if (received is not null && !string.IsNullOrEmpty(received.Provider))
                {
                    var progress = received with { Account = NormalizeAccount(received.Account) };
                    Post(() =>
                    {
                        _signInProgress[(progress.Provider, progress.Account)] = progress;
                        SignInProgress?.Invoke(progress);
                    });
                }
                break;
            }

            case "streambim_regions":
            {
                var regions = JsonSerializer.Deserialize(line, EngineJsonContext.Default.StreamBimRegionsInfo);
                if (regions is not null)
                {
                    Post(() =>
                    {
                        _streamBimRegions = regions;
                        StreamBimRegionsChanged?.Invoke(regions);
                    });
                }
                break;
            }

            case "activity":
            {
                // The whole list: on `hello`, and again when the window fell behind.
                var list = JsonSerializer.Deserialize(line, EngineJsonContext.Default.ActivityList);
                var items = list?.Items ?? new List<ActivityItem>();
                Post(() => ReplaceActivity(items));
                break;
            }

            case "activity_item":
            {
                // docs\IPC.md: {"type":"activity_item","item":{...}}. An item at the top
                // level is accepted as well, but only when it looks like one.
                var itemJson = root["item"] is JsonObject nested
                    ? nested.ToJsonString()
                    : root.ContainsKey("kind") ? line : null;
                var activityItem = itemJson is null
                    ? null
                    : JsonSerializer.Deserialize(itemJson, EngineJsonContext.Default.ActivityItem);
                if (activityItem is not null)
                {
                    Post(() =>
                    {
                        Activity.Insert(0, activityItem);
                        while (Activity.Count > ActivityLimit)
                        {
                            Activity.RemoveAt(Activity.Count - 1);
                        }
                    });
                }
                break;
            }

            case "notify":
            {
                var notify = JsonSerializer.Deserialize(line, EngineJsonContext.Default.NotifyInfo);
                if (notify is not null && !string.IsNullOrWhiteSpace(notify.Body ?? notify.Title))
                {
                    // Capture the active request before posting so a queued callback
                    // cannot clear a newer request. The protocol has no request ID, so
                    // a completion received after a timeout/new request is ambiguous.
                    var freeUpToken = IsFreeUpCompletion(notify.Body) ? CurrentFreeUpToken() : 0;
                    Post(() =>
                    {
                        if (freeUpToken != 0)
                        {
                            ClearFreeUp(freeUpToken);
                        }
                        Notified?.Invoke(this, notify);
                    });
                }
                break;
            }

            case "update":
            {
                var update = JsonSerializer.Deserialize(line, EngineJsonContext.Default.UpdateInfo);
                if (update is not null)
                {
                    Post(() =>
                    {
                        _update = update;
                        Raise(nameof(Update));
                    });
                }
                break;
            }
        }
    }

    /// <summary>
    /// Takes in a whole activity list. Items the window already holds keep their
    /// instance, so the pages keep their rows (and the list its scroll position)
    /// when the engine resends the same list after a reconnect. Runs on the UI thread.
    /// </summary>
    private void ReplaceActivity(List<ActivityItem> incoming)
    {
        var held = new Dictionary<ActivityItem, Queue<ActivityItem>>();
        foreach (var item in Activity)
        {
            if (!held.TryGetValue(item, out var same))
            {
                same = new Queue<ActivityItem>();
                held[item] = same;
            }
            same.Enqueue(item);
        }

        var next = new List<ActivityItem>(Math.Min(incoming.Count, ActivityLimit));
        foreach (var item in incoming)
        {
            if (item is null)
            {
                continue;
            }

            if (next.Count >= ActivityLimit)
            {
                break;
            }

            next.Add(held.TryGetValue(item, out var same) && same.Count > 0 ? same.Dequeue() : item);
        }

        if (next.Count == Activity.Count)
        {
            var unchanged = true;
            for (var i = 0; i < next.Count; i++)
            {
                if (!ReferenceEquals(next[i], Activity[i]))
                {
                    unchanged = false;
                    break;
                }
            }

            if (unchanged)
            {
                return;
            }
        }

        Activity.Clear();
        foreach (var item in next)
        {
            Activity.Add(item);
        }
    }

    // ------------------------------------------------------- convenience API

    /// <summary>
    /// Starts a browser sign-in: ACC (default) or Interaxo. The result comes as
    /// `sign_in_progress` and then `status`. StreamBIM has its own overload with credentials.
    /// </summary>
    public Task<Ack> SignInAsync(string provider = "acc") =>
        SendAsync("sign_in", new Dictionary<string, object?> { ["provider"] = provider });

    /// <summary>
    /// Starts a StreamBIM sign-in with e-mail and password on exactly one server: a region
    /// (its subdomain, such as "app") or a custom https address. Giving both or neither is
    /// refused here, before anything is sent. The password is only handed to the command: it is
    /// not kept in a field, logged or echoed back here. The caller clears its own copy of the
    /// password box. A StreamBIM account that asks for an MFA code reports `mfa_required` in
    /// <see cref="SignInProgress"/>, under that account's key.
    /// </summary>
    public Task<Ack> SignInStreamBimAsync(string? region, string? url, string username, string password)
    {
        if ((region is null) == (url is null))
        {
            return Task.FromResult(new Ack(null, false,
                Lang.T("Ange antingen en region eller en adress.", "Give either a region or an address.")));
        }

        var fields = new Dictionary<string, object?>
        {
            ["provider"] = "streambim",
            ["username"] = username,
            ["password"] = password,
        };
        if (region is not null)
        {
            fields["region"] = region;
        }
        else
        {
            fields["url"] = url;
        }
        return SendAsync("sign_in", fields);
    }

    /// <summary>Sends the six-digit code the user typed for a StreamBIM account that is waiting for MFA.</summary>
    public Task<Ack> SignInMfaAsync(string provider, string account, string code) =>
        SendAsync("sign_in_mfa", new Dictionary<string, object?>
        {
            ["provider"] = provider,
            ["account"] = account,
            ["code"] = code,
        });

    /// <summary>
    /// Cancels a pending browser sign-in or MFA wait for one provider. StreamBIM names its
    /// account; without one the engine cancels every pending StreamBIM sign-in.
    /// </summary>
    public Task<Ack> CancelSignInAsync(string provider, string? account = null)
    {
        var fields = new Dictionary<string, object?> { ["provider"] = provider };
        if (account is not null)
        {
            fields["account"] = account;
        }
        return SendAsync("cancel_sign_in", fields);
    }

    /// <summary>
    /// Signs out of one StreamBIM account (its key as `account`), of every account of one
    /// provider (provider only), or of everything (no arguments). An account is always a StreamBIM
    /// account, so a named account without a provider means StreamBIM.
    /// </summary>
    public Task<Ack> SignOutAsync(string? provider = null, string? account = null)
    {
        var fields = new Dictionary<string, object?>();
        if (provider is not null || account is not null)
        {
            fields["provider"] = provider ?? "streambim";
        }
        if (account is not null)
        {
            fields["account"] = account;
        }
        return SendAsync("sign_out", fields.Count == 0 ? null : fields);
    }

    /// <summary>Asks the engine for the StreamBIM region list. The answer is <see cref="StreamBimRegionsChanged"/>.</summary>
    public Task<Ack> RequestStreamBimRegionsAsync() => SendAsync("streambim_regions");

    public Task<Ack> SyncNowAsync() => SendAsync("sync_now");

    /// <summary>Opens the given folder in Explorer, or the sync root when no path is given.</summary>
    public Task<Ack> OpenFolderAsync(string? path = null) =>
        path is null
            ? SendAsync("open_folder")
            : SendAsync("open_folder", new Dictionary<string, object?> { ["path"] = path });

    public Task<Ack> OpenLogAsync() => SendAsync("open_log");

    public Task<Ack> RefreshProjectsAsync() => SendAsync("refresh_projects");

    /// <summary>Subscribes to a project (synced to the local folder) or unsubscribes from it.</summary>
    public Task<Ack> SetProjectAsync(string hubId, string projectId, bool subscribe) =>
        SendAsync("set_project", new Dictionary<string, object?>
        {
            ["hub_id"] = hubId,
            ["project_id"] = projectId,
            ["subscribe"] = subscribe,
        });

    /// <summary>Sets one setting. Keys are those of docs\IPC.md (set_setting).</summary>
    public Task<Ack> SetSettingAsync(string key, object? value) =>
        SendAsync("set_setting", new Dictionary<string, object?>
        {
            ["key"] = key,
            ["value"] = value,
        });

    public Task<Ack> TestApsClientIdAsync(string clientId, string requestId) =>
        SendAsync("test_aps_client_id", new Dictionary<string, object?>
        {
            ["client_id"] = clientId,
            ["request_id"] = requestId,
        });

    public Task<Ack> CancelApsClientIdTestAsync(string requestId) =>
        SendAsync("cancel_aps_client_id_test", new Dictionary<string, object?> { ["request_id"] = requestId });

    /// <summary>Frees clean local copies and clears pins in one project, or in all projects.</summary>
    public async Task<Ack> FreeUpSpaceAsync(string? projectId = null)
    {
        if (!TryBeginFreeUp(out var token))
        {
            return new Ack(null, false, Lang.T(
                "Frigör utrymme pågår redan. Vänta tills det är klart.",
                "Free up space is already running. Wait until it finishes."));
        }

        _ = ExpireFreeUpAsync(token);
        try
        {
            var ack = projectId is null
                ? await SendAsync("free_up_space")
                : await SendAsync("free_up_space", new Dictionary<string, object?> { ["project_id"] = projectId });
            if (!ack.Ok)
            {
                ClearFreeUp(token);
            }
            return ack;
        }
        catch (Exception exception)
        {
            ClearFreeUp(token);
            return new Ack(null, false, exception.Message);
        }
    }

    private bool TryBeginFreeUp(out int token)
    {
        lock (_freeUpGate)
        {
            if (_isFreeingUp)
            {
                token = 0;
                return false;
            }
            token = ++_freeUpGeneration;
            _isFreeingUp = true;
        }
        Post(() => Raise(nameof(IsFreeingUp)));
        return true;
    }

    private int CurrentFreeUpToken()
    {
        lock (_freeUpGate)
        {
            return _isFreeingUp ? _freeUpGeneration : 0;
        }
    }

    private void ClearFreeUp(int token = 0)
    {
        lock (_freeUpGate)
        {
            if (!_isFreeingUp || (token != 0 && _freeUpGeneration != token))
            {
                return;
            }
            _isFreeingUp = false;
        }
        Post(() => Raise(nameof(IsFreeingUp)));
    }

    private static bool IsFreeUpCompletion(string? body) =>
        body is not null &&
        (body.StartsWith("Frigjorde plats:", StringComparison.OrdinalIgnoreCase) ||
         body.StartsWith("Kunde inte frigöra plats:", StringComparison.OrdinalIgnoreCase));

    private async Task ExpireFreeUpAsync(int token)
    {
        await Task.Delay(FreeUpAnswerTimeout).ConfigureAwait(false);
        ClearFreeUp(token);
    }

    /// <summary>Puts the failed jobs back in the queue. The result shows in `status`.</summary>
    public Task<Ack> RetryFailedAsync() => SendAsync("retry_failed");

    /// <summary>Clears the activity list in the engine and every connected window.</summary>
    public Task<Ack> ClearActivityAsync() => SendAsync("clear_activity");

    /// <summary>The engine zips its logs and a sanitized summary to the desktop and says so in a notify.</summary>
    public Task<Ack> ExportDiagnosticsAsync() => SendAsync("export_diagnostics");

    /// <summary>
    /// Rebuilds a project's folder from ACC. Unsynced local changes are saved aside
    /// first, by the engine. The result comes as a notify and an activity item (kind "reset").
    /// </summary>
    public Task<Ack> ResetProjectAsync(string projectId) =>
        SendAsync("reset", new Dictionary<string, object?> { ["project_id"] = projectId });

    /// <summary>
    /// Rebuilds every subscribed project from ACC where it is, one after the other.
    /// Each project reports as its own notify.
    /// </summary>
    public Task<Ack> ResetAllAsync() => SendAsync("reset_all");

    /// <summary>
    /// Moves the sync root to <paramref name="path"/> (the new root itself, absolute) and
    /// rebuilds every project there from ACC. A place that cannot work is refused in the
    /// ack, with the engine's reason. Once accepted the engine restarts itself: the
    /// pipe closes, and the window waits for the engine to come back instead of
    /// reporting it gone (<see cref="EngineConnectionState.Restarting"/>).
    /// </summary>
    public async Task<Ack> MoveSyncRootAsync(string path)
    {
        // Marked before the command goes: the pipe may close soon after the ack.
        Interlocked.Exchange(ref _moveRequestedAt, Environment.TickCount64);
        var ack = await SendAsync("move_sync_root", new Dictionary<string, object?> { ["path"] = path });
        if (!ack.Ok)
        {
            Interlocked.Exchange(ref _moveRequestedAt, 0);
        }
        return ack;
    }

    /// <summary>
    /// Answers a held burst of deletes (status.held_deletes): true deletes them in ACC,
    /// false puts the files back locally.
    /// </summary>
    public Task<Ack> ConfirmDeletesAsync(string projectId, bool confirm) =>
        SendAsync("confirm_deletes", new Dictionary<string, object?>
        {
            ["project_id"] = projectId,
            ["confirm"] = confirm,
        });

    public Task<Ack> CheckUpdatesAsync() => SendAsync("check_updates");

    public Task<Ack> InstallUpdateAsync() => SendAsync("install_update");

    /// <summary>
    /// Stops the engine. The window will not start it again until the user asks
    /// (<see cref="StartEngine"/>); the engine's own startup task brings it back at
    /// the next sign-in.
    /// </summary>
    public async Task<Ack> QuitEngineAsync()
    {
        _quitByUser = true;
        var ack = await SendAsync("quit_engine");
        _quitByUser = ack.Ok && _quitByUser;
        return ack;
    }

    /// <summary>
    /// Stops the engine and starts it again once it has gone. Some settings
    /// (max_downloads, max_uploads, log_level) take effect only after a restart.
    /// </summary>
    public async Task<Ack> RestartEngineAsync()
    {
        _restartWhenGone = true;
        var ack = await QuitEngineAsync();
        if (!ack.Ok)
        {
            _restartWhenGone = false;
        }
        return ack;
    }

    /// <summary>
    /// Starts the engine on the user's request (the InfoBar's button). Does
    /// nothing while connected. Call on the UI thread.
    /// </summary>
    public void StartEngine()
    {
        if (_isConnected)
        {
            return;
        }

        _quitByUser = false;
        var now = Environment.TickCount64;
        Interlocked.Exchange(ref _startRequestedAt, now);
        Interlocked.Exchange(ref _lastLaunchAt, now);
        PublishState(EngineConnectionState.Starting, connected: false);
        _ = Task.Run(StartEngineProcess);
    }

    // ------------------------------------------------------------- the engine

    /// <summary>
    /// Starts the engine when nothing answers. Packaged: through the app's
    /// application entry, exactly as the Start menu would. Unpackaged (development):
    /// the file named by SYNKA_ENGINE, or the release build in the repo.
    /// </summary>
    private static void StartEngineProcess()
    {
        try
        {
            var family = PackageFamilyName();
            if (family is not null)
            {
                using var shell = Process.Start(new ProcessStartInfo("explorer.exe", $"shell:AppsFolder\\{family}!Engine")
                {
                    UseShellExecute = true,
                });
                return;
            }

            var exe = DevEnginePath();
            if (exe is null)
            {
                DiagnosticLog.Write("motorn hittades inte", null, new[]
                {
                    "Varken paketidentitet eller target\\release\\synka-engine.exe finns.",
                });
                return;
            }

            using var engine = Process.Start(new ProcessStartInfo(exe)
            {
                UseShellExecute = false,
                CreateNoWindow = true,
                WorkingDirectory = Path.GetDirectoryName(exe)!,
            });
        }
        catch (Exception exception)
        {
            DiagnosticLog.Write("start av motorn", exception);
        }
    }

    /// <summary>The package family name, or null when the window runs without package identity.</summary>
    private static string? PackageFamilyName()
    {
        try
        {
            return Package.Current.Id.FamilyName;
        }
        catch (Exception)
        {
            return null;
        }
    }

    /// <summary>The engine in a development checkout: SYNKA_ENGINE, else the nearest target\release above the window.</summary>
    private static string? DevEnginePath()
    {
        var fromEnvironment = Environment.GetEnvironmentVariable("SYNKA_ENGINE");
        if (!string.IsNullOrWhiteSpace(fromEnvironment) && File.Exists(fromEnvironment))
        {
            return fromEnvironment;
        }

        for (var directory = new DirectoryInfo(AppContext.BaseDirectory); directory is not null; directory = directory.Parent)
        {
            var candidate = Path.Combine(directory.FullName, "target", "release", "synka-engine.exe");
            if (File.Exists(candidate))
            {
                return candidate;
            }
        }

        return null;
    }

    // ------------------------------------------------------------ UI thread

    /// <summary>Runs the action on the UI thread. Inline when already there.</summary>
    private void Post(Action action)
    {
        var dispatcher = _dispatcher;
        if (dispatcher is null || dispatcher.HasThreadAccess)
        {
            action();
            return;
        }

        dispatcher.TryEnqueue(() => action());
    }

    private void Raise(string propertyName) =>
        PropertyChanged?.Invoke(this, new PropertyChangedEventArgs(propertyName));
}
