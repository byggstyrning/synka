using System.Diagnostics;
using System.IO.Pipes;
using System.Reflection;
using System.Security.Cryptography;
using System.Text;
using System.Text.Json;
using Synka.Services;

internal static class Program
{
    private static readonly TimeSpan IoDeadline = TimeSpan.FromSeconds(5);
    private static readonly BindingFlags PrivateInstance = BindingFlags.Instance | BindingFlags.NonPublic;

    public static async Task<int> Main(string[] args)
    {
        var scenarios = new (string Name, Func<Task<string>> Run)[]
        {
            ("fragmented_utf8_and_bad_frame_recovery", FragmentedUtf8AndBadFrameRecovery),
            ("out_of_order_command_acks", OutOfOrderCommandAcks),
            ("unterminated_eof_is_not_an_event", UnterminatedEofIsNotAnEvent),
            ("expired_unsent_commands_do_not_execute_later", ExpiredUnsentCommandsDoNotExecuteLater),
            ("disconnect_notifies_signed_in_bindings", DisconnectNotifiesSignedInBindings),
            ("rapid_commands_have_a_bounded_pending_queue", RapidCommandsHaveABoundedPendingQueue),
            ("oversized_command_is_rejected_before_transport", OversizedCommandIsRejectedBeforeTransport),
            ("aps_override_draft_survives_ack_reconnect_and_refusal", ApsOverrideDraftSurvivesAckReconnectAndRefusal),
            ("aps_override_survives_restart_before_ack", ApsOverrideSurvivesRestartBeforeAck),
            ("aps_validation_requires_correlated_result_and_survives_reconnect", ApsValidationRequiresCorrelatedResultAndSurvivesReconnect),
            ("activity_mime_mismatch_preserves_item_reason", ActivityMimeMismatchPreservesItemReason),
            ("activity_error_details_redact_credentials", ActivityErrorDetailsRedactCredentials),
            ("activity_local_paths_with_spaces_are_fully_redacted", ActivityLocalPathsWithSpacesAreFullyRedacted),
            ("provider_messages_parse_and_commands_carry_protocol_fields", ProviderMessagesParseAndCommandsCarryProtocolFields),
            ("providers_without_interaxo_are_not_offered", ProvidersWithoutInteraxoAreNotOffered),
            ("sign_in_progress_is_forgotten_across_connections", SignInProgressIsForgottenAcrossConnections),
            ("settings_retries_and_cancels_acc_and_interaxo_browser_sign_ins", SettingsRetriesAndCancelsBrowserSignInsPerProvider),
        };
        var selected = args.Length == 0 ? null : args[0];
        if (selected is not null && !scenarios.Any(s => s.Name == selected))
        {
            Console.Error.WriteLine("Unknown scenario: " + selected);
            return 2;
        }
        var results = new List<object>();
        var failures = 0;
        var started = DateTime.UtcNow;
        foreach (var scenario in scenarios.Where(s => selected is null || s.Name == selected))
        {
            var clock = Stopwatch.StartNew();
            try
            {
                var evidence = await scenario.Run();
                Console.WriteLine($"PASS {scenario.Name}: {evidence}");
                results.Add(new { scenario = scenario.Name, status = "passed", seconds = clock.Elapsed.TotalSeconds, evidence });
            }
            catch (Exception exception)
            {
                failures++;
                Console.WriteLine($"FAIL {scenario.Name}: {exception}");
                results.Add(new { scenario = scenario.Name, status = "failed", seconds = clock.Elapsed.TotalSeconds, evidence = exception.Message });
            }
        }
        var repo = FindRepo();
        var reportDirectory = Path.Combine(repo, "target", "ui-ipc-audit",
            $"{DateTime.UtcNow:yyyyMMdd-HHmmss}-{Environment.ProcessId}-{Guid.NewGuid():N}");
        Directory.CreateDirectory(reportDirectory);
        var reportPath = Path.Combine(reportDirectory, "summary.json");
        await File.WriteAllTextAsync(reportPath, JsonSerializer.Serialize(new
        {
            startedUtc = started,
            completedUtc = DateTime.UtcNow,
            engineClientSha256 = SourceHash(repo, "ui/Services/EngineClient.cs"),
            protocolSha256 = SourceHash(repo, "ui/Services/Protocol.cs"),
            errorDisplaySha256 = SourceHash(repo, "ui/Services/ErrorDisplay.cs"),
            apsEditStateSha256 = SourceHash(repo, "ui/Services/ApsClientIdEditState.cs"),
            platformStubs = "dispatcher runs inline; package identity unavailable; diagnostics and language are local stubs",
            transport = "real unique Windows named pipes; actual linked EngineClient and Protocol sources",
            failures,
            results,
        }, new JsonSerializerOptions { WriteIndented = true }));
        Console.WriteLine($"Report: {reportPath}");
        Console.WriteLine($"UI audit: {results.Count - failures} passed; {failures} failed;");
        return failures == 0 ? 0 : 1;
    }

    /// <summary>
    /// The multi-provider messages (accounts, hub provider, provider settings, sign-in progress,
    /// StreamBIM regions) parse, StreamBIM accounts are one per server (keyed), an older engine's
    /// status still gives the ACC account, and each new command carries exactly the fields of
    /// docs\IPC.md through the real pipe: sign_in with exactly one of region and url, and the
    /// account on sign_in_mfa, cancel_sign_in and sign_out.
    /// </summary>
    private static async Task<string> ProviderMessagesParseAndCommandsCarryProtocolFields()
    {
        using var pipes = await PipePair.Connect();
        var client = NewClient();
        var progress = new List<string>();
        client.SignInProgress += info => progress.Add(info.Provider + ":" + (info.Account ?? "-") + ":" + info.State);
        var serving = Serve(client, pipes.Window);
        await pipes.AckHello();

        // Two StreamBIM accounts, one per server: the default region (app) is signed in, a custom address is signing in.
        await pipes.Writer.WriteLineAsync("""{"type":"status","signed_in":true,"user":{"name":"Anna","email":"a@x.se"},"state":"synced","accounts":[{"provider":"acc","label":"ACC","configured":true,"signed_in":true,"signing_in":false,"user":{"name":"Anna","email":"a@x.se"},"host":null,"account":null},{"provider":"streambim","label":"StreamBIM","configured":true,"signed_in":true,"signing_in":false,"user":{"name":"Bo","email":"b@x.se"},"host":"app.streambim.com","account":"app"},{"provider":"streambim","label":"StreamBIM","configured":true,"signed_in":false,"signing_in":true,"user":null,"host":"streambim.example.se","account":"streambim.example.se"},{"provider":"interaxo","label":"Interaxo","configured":false,"signed_in":false,"signing_in":false,"user":null,"host":null,"account":null}]}""");
        await Until(() => client.Accounts.Count == 4);
        Require(client.IsSignedIn, "Status with accounts did not make the window signed in");
        Require(client.AccountFor("acc") is { SignedIn: true } acc && acc.User?.Name == "Anna" && acc.Account is null,
            "ACC account did not parse, or its account key is not null");
        Require(client.AccountFor("interaxo") is { Configured: false, SignedIn: false }, "Interaxo account did not parse");
        var streamBim = client.StreamBimAccounts;
        Require(streamBim.Count == 2
            && streamBim[0] is { Account: "app", SignedIn: true, Host: "app.streambim.com" } bo && bo.User?.Name == "Bo"
            && streamBim[1] is { Account: "streambim.example.se", SignedIn: false, SigningIn: true, Host: "streambim.example.se" },
            "StreamBIM accounts did not parse as one entry per server, in the engine's order, with their keys");

        await pipes.Writer.WriteLineAsync("""{"type":"projects","loading":false,"hubs":[{"id":"streambim:app","name":"StreamBIM","provider":"streambim","projects":[{"id":"streambim:app:1552","name":"Projekt","subscribed":true,"local_path":"C:/Synka/StreamBIM/Projekt"}]},{"id":"b.1","name":"Byggbolaget","projects":[]}]}""");
        await Until(() => client.Projects?.Hubs?.Count == 2);
        Require(client.Projects!.Hubs![0].Provider == "streambim" && client.Projects.Hubs[1].Provider is null,
            "Hub provider did not parse, or a hub without one is not null (ACC by default)");

        await pipes.Writer.WriteLineAsync("""{"type":"settings","propagate_deletes":false,"max_downloads":4,"max_uploads":2,"poll_active_secs":60,"log_level":"info","start_at_login":false,"sync_root":"C:/Synka","client_id_configured":true,"providers":["acc","streambim","interaxo"],"interaxo_client_id":"my-client","interaxo_redirect_uri":"http://localhost:47864/","interaxo_redirect_uri_default":"http://localhost:47864/","interaxo_client_secret_set":true}""");
        await Until(() => client.Settings?.InteraxoClientSecretSet == true);
        Require(client.Settings!.Providers?.Count == 3 && client.Settings.InteraxoClientId == "my-client"
            && client.Settings.InteraxoRedirectUriDefault == "http://localhost:47864/",
            "Provider settings did not parse");

        // The code prompt belongs to its account: the other StreamBIM account has no progress of its own.
        await pipes.Writer.WriteLineAsync("""{"type":"sign_in_progress","provider":"streambim","account":"streambim.example.se","state":"mfa_required","error":null}""");
        await Until(() => client.SignInProgressFor("streambim", "streambim.example.se")?.State == "mfa_required");
        Require(client.SignInProgressFor("streambim", "app") is null, "A code prompt of one StreamBIM account showed for another");
        Require(progress.Contains("streambim:streambim.example.se:mfa_required"), "sign_in_progress event was not raised with its account");

        await pipes.Writer.WriteLineAsync("""{"type":"streambim_regions","regions":[{"subdomain":"app","title":"Europa","is_default":true},{"subdomain":"sweden","title":"Sverige","is_default":false}]}""");
        await Until(() => client.StreamBimRegions?.Regions?.Count == 2);
        Require(client.StreamBimRegions!.Regions![0] is { Subdomain: "app", IsDefault: true }, "StreamBIM regions did not parse");

        // An older engine: no accounts. The ACC account comes from signed_in and user; StreamBIM is unknown,
        // and its progress carries no account (null), so it is kept under the provider alone.
        await pipes.Writer.WriteLineAsync("""{"type":"status","signed_in":true,"user":{"name":"Anna","email":"a@x.se"},"state":"synced"}""");
        await Until(() => client.Accounts.Count == 0 && client.Status?.State == "synced");
        Require(client.AccountFor("acc") is { SignedIn: true } older && older.User?.Email == "a@x.se",
            "Older engine: ACC account was not taken from signed_in and user");
        Require(client.AccountFor("streambim") is null && client.StreamBimAccounts.Count == 0,
            "Older engine: a StreamBIM account it does not report was not empty");
        await pipes.Writer.WriteLineAsync("""{"type":"sign_in_progress","provider":"streambim","state":"failed","error":"Fel"}""");
        await Until(() => client.SignInProgressFor("streambim")?.State == "failed");
        Require(client.SignInProgressFor("streambim", null) is { Account: null }, "Older engine: StreamBIM progress without an account was not kept without one");

        // sign_in: exactly one of region and url.
        var byRegion = await ExpectCommand(pipes, client.SignInStreamBimAsync("sweden", null, "b@x.se", "Hemligt-123"), "sign_in");
        Require(byRegion.GetProperty("provider").GetString() == "streambim" && byRegion.GetProperty("region").GetString() == "sweden"
            && !byRegion.TryGetProperty("url", out _)
            && byRegion.GetProperty("username").GetString() == "b@x.se" && byRegion.GetProperty("password").GetString() == "Hemligt-123",
            "sign_in with a region did not carry provider, region, username and password, and no url");

        var byUrl = await ExpectCommand(pipes, client.SignInStreamBimAsync(null, "https://streambim.example.se", "c@x.se", "Hemligt-456"), "sign_in");
        Require(byUrl.GetProperty("provider").GetString() == "streambim" && byUrl.GetProperty("url").GetString() == "https://streambim.example.se"
            && !byUrl.TryGetProperty("region", out _) && byUrl.GetProperty("username").GetString() == "c@x.se",
            "sign_in with a url did not carry the url, and no region");

        // Both or neither is refused before anything is sent: the next command on the pipe is the one after them.
        var both = await client.SignInStreamBimAsync("app", "https://streambim.example.se", "d@x.se", "Hemligt-789");
        var neither = await client.SignInStreamBimAsync(null, null, "d@x.se", "Hemligt-789");
        Require(!both.Ok && !neither.Ok, "sign_in with both a region and a url, or with neither, was not refused");

        var mfa = await ExpectCommand(pipes, client.SignInMfaAsync("streambim", "streambim.example.se", "012345"), "sign_in_mfa");
        Require(mfa.GetProperty("provider").GetString() == "streambim" && mfa.GetProperty("account").GetString() == "streambim.example.se"
            && mfa.GetProperty("code").GetString() == "012345",
            "sign_in_mfa did not send the account and the code as a string with its leading zero");

        var cancel = await ExpectCommand(pipes, client.CancelSignInAsync("streambim", "streambim.example.se"), "cancel_sign_in");
        Require(cancel.GetProperty("provider").GetString() == "streambim" && cancel.GetProperty("account").GetString() == "streambim.example.se",
            "cancel_sign_in did not name the provider and the account");

        var cancelAll = await ExpectCommand(pipes, client.CancelSignInAsync("streambim"), "cancel_sign_in");
        Require(cancelAll.GetProperty("provider").GetString() == "streambim" && !cancelAll.TryGetProperty("account", out _),
            "cancel_sign_in without an account sent one");

        var signOutOne = await ExpectCommand(pipes, client.SignOutAsync("streambim", "app"), "sign_out");
        Require(signOutOne.GetProperty("provider").GetString() == "streambim" && signOutOne.GetProperty("account").GetString() == "app",
            "sign_out for one StreamBIM account did not name the provider and the account");

        var signOutStreamBim = await ExpectCommand(pipes, client.SignOutAsync("streambim"), "sign_out");
        Require(signOutStreamBim.GetProperty("provider").GetString() == "streambim" && !signOutStreamBim.TryGetProperty("account", out _),
            "sign_out for a provider sent an account");

        var signOutAll = await ExpectCommand(pipes, client.SignOutAsync(), "sign_out");
        Require(!signOutAll.TryGetProperty("provider", out _) && !signOutAll.TryGetProperty("account", out _),
            "sign_out without a provider sent one");

        var signOutInteraxo = await ExpectCommand(pipes, client.SignOutAsync("interaxo"), "sign_out");
        Require(signOutInteraxo.GetProperty("provider").GetString() == "interaxo" && !signOutInteraxo.TryGetProperty("account", out _),
            "sign_out did not name the provider");

        var signIn = await ExpectCommand(pipes, client.SignInAsync(), "sign_in");
        Require(signIn.GetProperty("provider").GetString() == "acc", "sign_in without an argument did not mean ACC");

        await ExpectCommand(pipes, client.RequestStreamBimRegionsAsync(), "streambim_regions");

        return "Two StreamBIM accounts parsed per server with their keys; per-account sign-in progress; an older status fell back to ACC; "
            + "sign_in with region or url (never both, refused locally when both or neither), sign_in_mfa, cancel_sign_in and sign_out "
            + "(per account, per provider, all) and streambim_regions sent their protocol fields";
    }

    /// <summary>
    /// Interaxo turned off (settings.json): the engine's `providers` names only ACC and StreamBIM and its status
    /// has no Interaxo account, so the window offers no Interaxo sign-in. An engine that sends no `providers`
    /// (older) offers every provider, as it did before.
    /// </summary>
    private static async Task<string> ProvidersWithoutInteraxoAreNotOffered()
    {
        using var pipes = await PipePair.Connect();
        var client = NewClient();
        var serving = Serve(client, pipes.Window);
        await pipes.AckHello();

        await pipes.Writer.WriteLineAsync("""{"type":"status","signed_in":true,"user":{"name":"Anna","email":"a@x.se"},"state":"synced","accounts":[{"provider":"acc","label":"ACC","configured":true,"signed_in":true,"signing_in":false,"user":{"name":"Anna","email":"a@x.se"},"host":null,"account":null}]}""");
        await Until(() => client.Accounts.Count == 1);
        Require(client.AccountFor("interaxo") is null, "An account of a provider the engine left out was reported");

        await pipes.Writer.WriteLineAsync("""{"type":"settings","propagate_deletes":false,"max_downloads":4,"max_uploads":2,"poll_active_secs":60,"log_level":"info","start_at_login":false,"sync_root":"C:/Synka","client_id_configured":true,"providers":["acc","streambim"],"interaxo_client_id":null,"interaxo_redirect_uri":"http://localhost:47864/","interaxo_redirect_uri_default":"http://localhost:47864/","interaxo_client_secret_set":false}""");
        await Until(() => client.Settings?.Providers?.Count == 2);
        Require(client.ProviderOffered("acc") && client.ProviderOffered("streambim") && !client.ProviderOffered("interaxo"),
            "providers without interaxo did not leave Interaxo out of the offer");

        await pipes.Writer.WriteLineAsync("""{"type":"settings","propagate_deletes":false,"max_downloads":4,"max_uploads":2,"poll_active_secs":60,"log_level":"info","start_at_login":false,"sync_root":"C:/Synka","client_id_configured":true}""");
        await Until(() => client.Settings?.Providers is null);
        Require(client.ProviderOffered("interaxo"), "An engine that sends no providers did not offer Interaxo as before");

        pipes.Engine.Dispose();
        await serving.WaitAsync(IoDeadline);
        return "A status and settings without Interaxo left it out of the offer; an engine without providers offers every provider";
    }

    /// <summary>
    /// The cached sign-in progress belongs to one connection: a disconnect and a reconnect both forget
    /// it, so a StreamBIM code prompt from an engine that is gone is not shown again. Each StreamBIM
    /// account's prompt is forgotten the same way (here the account key is "app").
    /// </summary>
    private static Task<string> SignInProgressIsForgottenAcrossConnections()
    {
        var client = NewClient();
        var handle = typeof(EngineClient).GetMethod("HandleLine", PrivateInstance)!;
        var publish = typeof(EngineClient).GetMethod("PublishState", PrivateInstance)!;
        const string prompt = """{"type":"sign_in_progress","provider":"streambim","account":"app","state":"mfa_required","error":null}""";

        publish.Invoke(client, new object[] { EngineConnectionState.Connected, true });
        handle.Invoke(client, new object[] { prompt });
        Require(client.SignInProgressFor("streambim", "app")?.State == "mfa_required", "The MFA prompt was not kept while connected");

        publish.Invoke(client, new object[] { EngineConnectionState.Disconnected, false });
        Require(client.SignInProgressFor("streambim", "app") is null, "A disconnect kept the cached MFA prompt");

        handle.Invoke(client, new object[] { prompt });
        publish.Invoke(client, new object[] { EngineConnectionState.Connected, true });
        Require(client.SignInProgressFor("streambim", "app") is null, "A reconnect kept the cached MFA prompt");

        return Task.FromResult("A prompt was kept while connected and forgotten by the disconnect and by the reconnect");
    }

    /// <summary>
    /// Settings retries an ACC or Interaxo sign-in while its browser is open (sign_in again, which the
    /// engine answers by replacing the pending attempt), and Avbryt sends cancel_sign_in for that provider only.
    /// </summary>
    private static async Task<string> SettingsRetriesAndCancelsBrowserSignInsPerProvider()
    {
        using var pipes = await PipePair.Connect();
        var client = NewClient();
        _ = Serve(client, pipes.Window);
        await pipes.AckHello();
        await pipes.Writer.WriteLineAsync("""{"type":"status","signed_in":false,"user":null,"state":"signed_out","accounts":[{"provider":"acc","label":"ACC","configured":true,"signed_in":false,"signing_in":true,"user":null,"host":null},{"provider":"streambim","label":"StreamBIM","configured":true,"signed_in":false,"signing_in":false,"user":null,"host":null},{"provider":"interaxo","label":"Interaxo","configured":true,"signed_in":false,"signing_in":true,"user":null,"host":null}]}""");
        await Until(() => client.Accounts.Count == 3);
        Require(client.AccountFor("acc") is { SigningIn: true, SignedIn: false }, "ACC was not seen signing in");

        var retry = await ExpectCommand(pipes, client.SignInAsync("acc"), "sign_in");
        Require(retry.GetProperty("provider").GetString() == "acc", "A retry while the ACC browser is open did not name ACC");

        var cancelAcc = await ExpectCommand(pipes, client.CancelSignInAsync("acc"), "cancel_sign_in");
        Require(cancelAcc.GetProperty("provider").GetString() == "acc", "cancel_sign_in did not name ACC");

        await pipes.Writer.WriteLineAsync("""{"type":"sign_in_progress","provider":"acc","state":"cancelled","error":null}""");
        await Until(() => client.SignInProgressFor("acc")?.State == "cancelled");

        var cancelInteraxo = await ExpectCommand(pipes, client.CancelSignInAsync("interaxo"), "cancel_sign_in");
        Require(cancelInteraxo.GetProperty("provider").GetString() == "interaxo", "cancel_sign_in did not name Interaxo");

        return "A retry sent sign_in for ACC while signing in; cancel_sign_in named ACC, then Interaxo, and the cancelled progress was kept per provider";
    }

    /// <summary>Takes the next command the client writes, checks its name, acks it, and returns it.</summary>
    private static async Task<JsonElement> ExpectCommand(PipePair pipes, Task<Ack> call, string cmd)
    {
        using var document = JsonDocument.Parse(await pipes.Read());
        var root = document.RootElement.Clone();
        var name = root.GetProperty("cmd").GetString();
        Require(name == cmd, $"Expected the command {cmd}, the client sent {name}");
        await pipes.Writer.WriteLineAsync(JsonSerializer.Serialize(new { type = "ack", id = root.GetProperty("id").GetInt64(), ok = true }));
        var ack = await call;
        Require(ack.Ok, $"{cmd} was not acknowledged: {ack.Error}");
        return root;
    }

    private static string FindRepo()
    {
        for (var directory = new DirectoryInfo(AppContext.BaseDirectory); directory is not null; directory = directory.Parent)
        {
            if (File.Exists(Path.Combine(directory.FullName, "Cargo.toml"))) return directory.FullName;
        }
        throw new InvalidOperationException("Cannot locate the Synka source tree");
    }

    private static string SourceHash(string repo, string path) =>
        Convert.ToHexString(SHA256.HashData(File.ReadAllBytes(Path.Combine(repo, path))));

    private static EngineClient NewClient() =>
        (EngineClient)Activator.CreateInstance(typeof(EngineClient), nonPublic: true)!;

    private static Task Serve(EngineClient client, NamedPipeClientStream pipe) =>
        (Task)typeof(EngineClient).GetMethod("ServeAsync", PrivateInstance)!.Invoke(client, new object[] { pipe })!;

    private static async Task Until(Func<bool> condition)
    {
        using var timeout = new CancellationTokenSource(IoDeadline);
        while (!condition()) await Task.Delay(5, timeout.Token);
    }

    private static void Require(bool condition, string evidence)
    {
        if (!condition) throw new InvalidOperationException(evidence);
    }

    private sealed class PipePair : IDisposable
    {
        public readonly NamedPipeServerStream Engine;
        public readonly NamedPipeClientStream Window;
        public readonly StreamReader Reader;
        public readonly StreamWriter Writer;

        private PipePair(NamedPipeServerStream engine, NamedPipeClientStream window)
        {
            Engine = engine;
            Window = window;
            Reader = new StreamReader(engine, new UTF8Encoding(false), false, 4096, leaveOpen: true);
            Writer = new StreamWriter(engine, new UTF8Encoding(false), 4096, leaveOpen: true) { AutoFlush = true, NewLine = "\n" };
        }

        public static async Task<PipePair> Connect()
        {
            var name = $"synka-ui-audit-{Environment.ProcessId}-{Guid.NewGuid():N}";
            var engine = new NamedPipeServerStream(name, PipeDirection.InOut, 1, PipeTransmissionMode.Byte,
                PipeOptions.Asynchronous, 4096, 4096);
            var window = new NamedPipeClientStream(".", name, PipeDirection.InOut, PipeOptions.Asynchronous);
            try
            {
                await Task.WhenAll(engine.WaitForConnectionAsync(), window.ConnectAsync()).WaitAsync(IoDeadline);
                return new PipePair(engine, window);
            }
            catch
            {
                engine.Dispose();
                window.Dispose();
                throw;
            }
        }

        public async Task<string> Read() =>
            await Reader.ReadLineAsync().WaitAsync(IoDeadline) ?? throw new EndOfStreamException("Client closed the test pipe");

        public async Task AckHello()
        {
            using var hello = JsonDocument.Parse(await Read());
            Require(hello.RootElement.GetProperty("cmd").GetString() == "hello", "Production client did not send hello");
            await Writer.WriteLineAsync(JsonSerializer.Serialize(new { type = "ack", id = hello.RootElement.GetProperty("id").GetInt64(), ok = true }));
        }

        public void Dispose()
        {
            // Dispose the transport first: any deliberately blocked writer must be released.
            Window.Dispose();
            Engine.Dispose();
            Reader.Dispose();
            try { Writer.Dispose(); } catch (Exception exception) when (exception is IOException or ObjectDisposedException) { }
        }
    }

    private static async Task<string> FragmentedUtf8AndBadFrameRecovery()
    {
        using var pipes = await PipePair.Connect();
        var client = NewClient();
        var serving = Serve(client, pipes.Window);
        await pipes.AckHello();
        var bytes = Encoding.UTF8.GetBytes("{\"type\":\"status\",\"signed_in\":true,\"text\":\"ritning 🏗 åäö\"}\n");
        // Split every UTF-8 sequence and JSON token over actual kernel pipe writes.
        foreach (var value in bytes) await pipes.Engine.WriteAsync(new[] { value });
        await Until(() => client.Status?.Text == "ritning 🏗 åäö");
        await pipes.Writer.WriteLineAsync("{invalid JSON}");
        await pipes.Writer.WriteLineAsync("{\"type\":\"status\",\"signed_in\":false,\"text\":\"recovered\"}");
        await Until(() => client.Status?.Text == "recovered");
        pipes.Engine.Dispose();
        await serving.WaitAsync(IoDeadline);
        return "A split Unicode status survived; malformed JSON did not prevent the following status";
    }

    private static async Task<string> OutOfOrderCommandAcks()
    {
        using var pipes = await PipePair.Connect();
        var client = NewClient();
        var serving = Serve(client, pipes.Window);
        await pipes.AckHello();
        var calls = Enumerable.Range(0, 16).Select(_ => client.SyncNowAsync()).ToArray();
        var ids = new List<long>();
        foreach (var _ in calls)
        {
            using var command = JsonDocument.Parse(await pipes.Read());
            ids.Add(command.RootElement.GetProperty("id").GetInt64());
        }
        foreach (var id in ids.AsEnumerable().Reverse())
            await pipes.Writer.WriteLineAsync(JsonSerializer.Serialize(new { type = "ack", id, ok = id % 2 == 0, error = "injected refusal" }));
        var acknowledgments = await Task.WhenAll(calls).WaitAsync(IoDeadline);
        for (var i = 0; i < ids.Count; i++)
            Require(acknowledgments[i].Id == ids[i] && acknowledgments[i].Ok == (ids[i] % 2 == 0),
                "A reversed ACK was delivered to the wrong caller");
        pipes.Engine.Dispose();
        await serving.WaitAsync(IoDeadline);
        return "16 simultaneous commands correlated success and error ACKs received in reverse order";
    }

    private static async Task<string> UnterminatedEofIsNotAnEvent()
    {
        using var pipes = await PipePair.Connect();
        var client = NewClient();
        var serving = Serve(client, pipes.Window);
        await pipes.AckHello();
        var incomplete = Encoding.UTF8.GetBytes("{\"type\":\"status\",\"signed_in\":true,\"text\":\"unterminated stale status\"}");
        await pipes.Engine.WriteAsync(incomplete);
        await pipes.Engine.FlushAsync();
        pipes.Engine.Dispose();
        await serving.WaitAsync(IoDeadline);
        Require(client.Status is null,
            $"Production ServeAsync applied status='{client.Status?.Text}' from a frame with no terminating newline at disconnect");
        return "An incomplete frame at EOF was discarded";
    }

    private static async Task<string> ExpiredUnsentCommandsDoNotExecuteLater()
    {
        using var pipes = await PipePair.Connect();
        var client = NewClient();
        // Match ServeAsync's writer setup, but hold the engine reader to simulate a hung
        // engine. The real SendAsync, semaphore, timeout, writer and OS pipe are unchanged.
        var writer = new StreamWriter(pipes.Window, new UTF8Encoding(false), 1 << 16, leaveOpen: true)
            { AutoFlush = true, NewLine = "\n" };
        typeof(EngineClient).GetField("_writer", PrivateInstance)!.SetValue(client, writer);
        var calls = Enumerable.Range(0, 8).Select(index => client.SendAsync("set_project", new Dictionary<string, object?>
        {
            ["hub_id"] = "private-fake-hub",
            ["project_id"] = $"private-fake-project-{index}",
            ["subscribe"] = false,
            ["padding"] = new string('x', 32 * 1024),
        })).ToArray();
        var acks = await Task.WhenAll(calls).WaitAsync(TimeSpan.FromSeconds(20));
        Require(acks.All(ack => !ack.Ok), "The fake engine never ACKed, so no command may report success");
        var receivedAfterTimeout = new List<long>();
        var unfinished = new StringBuilder();
        var buffer = new char[4096];
        while (receivedAfterTimeout.Count < calls.Length)
        {
            using var idle = new CancellationTokenSource(TimeSpan.FromMilliseconds(500));
            int read;
            try { read = await pipes.Reader.ReadAsync(buffer.AsMemory(), idle.Token); }
            catch (Exception exception) when (exception is OperationCanceledException or IOException) { break; }
            if (read == 0) break;
            for (var index = 0; index < read; index++)
            {
                if (buffer[index] != '\n') { unfinished.Append(buffer[index]); continue; }
                using var command = JsonDocument.Parse(unfinished.ToString());
                receivedAfterTimeout.Add(command.RootElement.GetProperty("id").GetInt64());
                unfinished.Clear();
            }
        }
        pipes.Window.Dispose();
        await Until(() => ((SemaphoreSlim)typeof(EngineClient).GetField("_sendLock", PrivateInstance)!.GetValue(client)!).CurrentCount == 1);
        try { writer.Dispose(); } catch (Exception exception) when (exception is IOException or ObjectDisposedException) { }
        // One write may already have entered the kernel before timeout. The seven commands
        // waiting for the send semaphore have not started and must expire with their callers.
        Require(receivedAfterTimeout.Count <= 1,
            $"All callers timed out, then the real client sent {receivedAfterTimeout.Count}/8 commands when the fake engine resumed; IDs={string.Join(',', receivedAfterTimeout)}. Expired semaphore waiters still execute");
        return $"Only {receivedAfterTimeout.Count} complete already-started writes remained after timeout; incomplete tail chars={unfinished.Length}";
    }


    private static async Task<string> ApsOverrideDraftSurvivesAckReconnectAndRefusal()
    {
        var client = NewClient();
        var edit = client.ApsClientIdEdit;
        var settingsSeen = 0;
        client.PropertyChanged += (_, args) =>
        {
            if (args.PropertyName != nameof(EngineClient.Settings)) return;
            if (client.Settings is { ApsClientIdOverrideSupported: true } settings)
                edit.ObserveSettings(settings.ApsClientId);
            Interlocked.Increment(ref settingsSeen);
        };
        client.Notified += (_, notify) =>
        {
            if (notify.Severity is "warning" or "error"
                && notify.Body?.Contains("APS", StringComparison.OrdinalIgnoreCase) == true)
                edit.FailIfPending(edit.RequestVersion, "engine refused APS change");
        };

        static string SettingsFrame(bool capability, string? value)
        {
            var fields = new Dictionary<string, object?>
            {
                ["type"] = "settings", ["max_downloads"] = 4, ["max_uploads"] = 2,
                ["poll_active_secs"] = 30, ["log_level"] = "info", ["auto_update"] = true,
                ["update_channel_configured"] = false,
            };
            if (capability)
            {
                fields["aps_client_id_override_supported"] = true;
                fields["aps_client_id_validation_supported"] = true;
                fields["aps_client_id"] = value;
            }
            return JsonSerializer.Serialize(fields);
        }

        async Task Snapshot(PipePair pipes, bool capability, string? value)
        {
            var before = Volatile.Read(ref settingsSeen);
            await pipes.Writer.WriteLineAsync(SettingsFrame(capability, value));
            await Until(() => Volatile.Read(ref settingsSeen) > before);
        }

        async Task ApplyAndAck(PipePair pipes, string value)
        {
            var sent = value.Length > 0
                ? client.TestApsClientIdAsync(value, edit.RequestId!)
                : client.SetSettingAsync("aps_client_id", value);
            using var command = JsonDocument.Parse(await pipes.Read());
            Require(value.Length > 0
                ? command.RootElement.GetProperty("cmd").GetString() == "test_aps_client_id"
                    && command.RootElement.GetProperty("client_id").GetString() == value
                    && command.RootElement.GetProperty("request_id").GetString() == edit.RequestId
                : command.RootElement.GetProperty("cmd").GetString() == "set_setting"
                    && command.RootElement.GetProperty("key").GetString() == "aps_client_id"
                    && command.RootElement.GetProperty("value").GetString() == value,
                "The actual client did not send the public APS setting correctly");
            await pipes.Writer.WriteLineAsync(JsonSerializer.Serialize(new
            {
                type = "ack", id = command.RootElement.GetProperty("id").GetInt64(), ok = true,
            }));
            Require((await sent.WaitAsync(IoDeadline)).Ok, "The synthetic enqueue ACK was not correlated");
        }

        using (var first = await PipePair.Connect())
        {
            var serving = Serve(client, first.Window);
            await first.AckHello();
            await Snapshot(first, capability: false, value: null);
            Require(client.Settings?.ApsClientIdOverrideSupported is null && !edit.Initialized,
                "An older engine unexpectedly advertised the override editor");
            await Snapshot(first, capability: true, value: null);
            Require(edit.Initialized && edit.Confirmed == string.Empty, "The default APS app was not recognized");

            edit.Edit("  private-public-client-id  ");
            await Snapshot(first, capability: true, value: null);
            Require(edit.Draft == "  private-public-client-id  ", "A routine snapshot erased the typed draft");
            edit.Begin(edit.Draft, validate: true);
            await ApplyAndAck(first, ApsClientIdEditState.Normalize(edit.Draft));
            Require(edit.IsWaiting && edit.Draft == "  private-public-client-id  ",
                "An early enqueue ACK was treated as persistence confirmation");
            await Snapshot(first, capability: true, value: null);
            Require(edit.IsWaiting && edit.Draft == "  private-public-client-id  ",
                "An unrelated old snapshot erased or confirmed the pending change");
            first.Engine.Dispose();
            await serving.WaitAsync(IoDeadline);
            Require(edit.IsWaiting, "Disconnect silently confirmed an unobserved setting");
        }

        using (var second = await PipePair.Connect())
        {
            var serving = Serve(client, second.Window);
            await second.AckHello();
            await Snapshot(second, capability: true, value: "private-public-client-id");
            Require(!edit.IsWaiting && edit.Draft == "private-public-client-id"
                && edit.Confirmed == "private-public-client-id", "Reconnect snapshot did not confirm the actual persisted value");

            edit.Edit("keep-this-unsaved-draft");
            var rejected = edit.Begin(string.Empty);
            await ApplyAndAck(second, string.Empty);
            Require(edit.Draft == "keep-this-unsaved-draft" && edit.IsWaiting,
                "Use default erased the draft before the engine confirmed it");
            await second.Writer.WriteLineAsync(JsonSerializer.Serialize(new
            {
                type = "notify", title = "Private audit", body = "APS setting refused while busy", severity = "warning",
            }));
            await Until(() => !edit.IsWaiting);
            Require(edit.Draft == "keep-this-unsaved-draft" && edit.Problem is not null,
                "An asynchronous APS refusal erased the recoverable draft");
            await Snapshot(second, capability: true, value: "private-public-client-id");
            Require(edit.Draft == "keep-this-unsaved-draft", "A post-refusal snapshot erased the draft");

            var retry = edit.Begin(string.Empty);
            Require(!edit.FailIfPending(rejected, "old timeout") && edit.IsPending(retry),
                "A stale timeout cleared the newer save request");
            await ApplyAndAck(second, string.Empty);
            await Snapshot(second, capability: true, value: null);
            Require(!edit.IsWaiting && edit.Confirmed == string.Empty && edit.Draft == string.Empty,
                "Confirmed Synka default did not clear the custom draft");
            second.Engine.Dispose();
            await serving.WaitAsync(IoDeadline);
        }
        return "Actual protocol + edit state preserved drafts across snapshots, early ACK, reconnect, refusal and retry; old engines omit the capability";
    }

    private static async Task<string> ApsOverrideSurvivesRestartBeforeAck()
    {
        var client = NewClient();
        var edit = client.ApsClientIdEdit;
        var settingsSeen = 0;
        client.PropertyChanged += (_, args) =>
        {
            if (args.PropertyName != nameof(EngineClient.Settings)
                || client.Settings is not { ApsClientIdOverrideSupported: true } settings) return;
            edit.ObserveSettings(settings.ApsClientId);
            Interlocked.Increment(ref settingsSeen);
        };

        async Task Snapshot(PipePair pipes, string? value)
        {
            var before = Volatile.Read(ref settingsSeen);
            await pipes.Writer.WriteLineAsync(JsonSerializer.Serialize(new
            {
                type = "settings", aps_client_id_override_supported = true, aps_client_id = value,
            }));
            await Until(() => Volatile.Read(ref settingsSeen) > before);
        }

        using (var first = await PipePair.Connect())
        {
            var serving = Serve(client, first.Window);
            await first.AckHello();
            await Snapshot(first, null);
            edit.Edit("public-client-saved-before-restart");
            var request = edit.Begin(edit.Draft, validate: true);
            var sending = client.TestApsClientIdAsync(edit.Draft, edit.RequestId!);
            using var accepted = JsonDocument.Parse(await first.Read());
            Require(accepted.RootElement.GetProperty("cmd").GetString() == "test_aps_client_id"
                && accepted.RootElement.GetProperty("client_id").GetString() == edit.PendingValue,
                "The test engine did not receive the setting before restart");

            // Model persistence followed by process shutdown before either ACK or settings
            // can reach this pipe. The next process sends the saved value in its snapshot.
            first.Engine.Dispose();
            await serving.WaitAsync(IoDeadline);
            var uncertain = await sending.WaitAsync(IoDeadline);
            Require(!uncertain.Ok && !uncertain.IsEngineResponse,
                "A transport disconnect was incorrectly tagged as an engine refusal");
            Require(!edit.ObserveAck(request, uncertain) && edit.IsPending(request)
                && edit.Problem is null && edit.Draft == "public-client-saved-before-restart",
                "Losing the ACK falsely rejected a possibly persisted setting");
        }

        using (var second = await PipePair.Connect())
        {
            var serving = Serve(client, second.Window);
            await second.AckHello();
            await Snapshot(second, "public-client-saved-before-restart");
            Require(!edit.IsWaiting && edit.Confirmed == "public-client-saved-before-restart"
                && edit.Problem is null, "Reconnect did not confirm the setting saved before ACK loss");

            edit.Edit("malformed.client.id");
            var request = edit.Begin(edit.Draft, validate: true);
            var sending = client.TestApsClientIdAsync(edit.Draft, edit.RequestId!);
            using var command = JsonDocument.Parse(await second.Read());
            await second.Writer.WriteLineAsync(JsonSerializer.Serialize(new
            {
                type = "ack", id = command.RootElement.GetProperty("id").GetInt64(),
                ok = false, error = "invalid APS client ID", is_engine_response = false,
            }));
            var refused = await sending.WaitAsync(IoDeadline);
            Require(!refused.Ok && refused.IsEngineResponse,
                "A genuine engine rejection did not carry local response-origin metadata");
            Require(edit.ObserveAck(request, refused) && !edit.IsWaiting
                && edit.Draft == "malformed.client.id" && edit.Problem == "invalid APS client ID",
                "An explicit engine refusal did not immediately release the draft for correction");
            var wireAck = JsonSerializer.Serialize(refused, EngineJsonContext.Default.Ack);
            Require(!wireAck.Contains("is_engine_response", StringComparison.Ordinal),
                "Local response-origin metadata leaked into the wire protocol");
            second.Engine.Dispose();
            await serving.WaitAsync(IoDeadline);
        }

        var localFailure = await client.SetSettingAsync("aps_client_id", "unused-client").WaitAsync(IoDeadline);
        Require(!localFailure.Ok && !localFailure.IsEngineResponse,
            "A disconnected local failure was incorrectly tagged as an engine response");
        return "ACK lost after a real-pipe command remained pending until reconnect confirmed persistence; explicit refusal stayed immediate; origin metadata stayed off wire";
    }
    private static async Task<string> ApsValidationRequiresCorrelatedResultAndSurvivesReconnect()
    {
        var client = NewClient();
        var edit = client.ApsClientIdEdit;
        static string Snapshot() => JsonSerializer.Serialize(new
        {
            type = "settings", aps_client_id_override_supported = true,
            aps_client_id_validation_supported = true, aps_client_id = "same-public-client",
        });
        static string Result(string requestId, string state, string? error = null) =>
            JsonSerializer.Serialize(new { type = "aps_client_id_validation", request_id = requestId, state, error });
        string pendingId;
        using (var first = await PipePair.Connect())
        {
            var serving = Serve(client, first.Window);
            await first.AckHello();
            await first.Writer.WriteLineAsync(Snapshot());
            await Until(() => edit.Initialized);
            Require(client.Settings?.ApsClientIdValidationSupported == true, "New validation capability did not deserialize");
            Require(edit.CanSave, "An unchanged custom ID could not be re-tested");
            edit.Begin(edit.Draft, validate: true);
            pendingId = edit.RequestId!;
            var sending = client.TestApsClientIdAsync(edit.Draft, pendingId);
            using var command = JsonDocument.Parse(await first.Read());
            Require(command.RootElement.GetProperty("request_id").GetString() == pendingId, "Operation ID was lost on the pipe");
            await first.Writer.WriteLineAsync(JsonSerializer.Serialize(new { type = "ack", id = command.RootElement.GetProperty("id").GetInt64(), ok = true }));
            Require((await sending).Ok && edit.IsTesting, "An enqueue ACK falsely completed browser validation");
            await first.Writer.WriteLineAsync(Snapshot());
            await first.Writer.WriteLineAsync(Result(Guid.NewGuid().ToString("D"), "saved"));
            await Until(() => client.ApsClientIdValidation is { State: "saved" });
            Require(edit.IsTesting && !edit.Saved, "Same-value snapshot or unrelated result falsely passed validation");
            first.Engine.Dispose();
            await serving.WaitAsync(IoDeadline);
        }
        using (var second = await PipePair.Connect())
        {
            var serving = Serve(client, second.Window);
            await second.AckHello();
            await second.Writer.WriteLineAsync(Snapshot());
            await second.Writer.WriteLineAsync(Result(pendingId, "failed", "HTTP 401: use Desktop, Mobile, Single-Page App"));
            await Until(() => !edit.IsWaiting);
            Require(edit.Confirmed == "same-public-client" && edit.Draft == "same-public-client"
                && edit.Problem?.Contains("401") == true && !edit.Saved,
                "Replay of a rejected test erased the setting/draft or claimed success");

            edit.Edit("new-unsaved-candidate");
            edit.Begin(edit.Draft, validate: true);
            var cancelId = edit.RequestId!;
            var cancelling = client.CancelApsClientIdTestAsync(cancelId);
            using var cancel = JsonDocument.Parse(await second.Read());
            Require(cancel.RootElement.GetProperty("cmd").GetString() == "cancel_aps_client_id_test"
                && cancel.RootElement.GetProperty("request_id").GetString() == cancelId,
                "Cancellation did not identify the browser attempt");
            await second.Writer.WriteLineAsync(JsonSerializer.Serialize(new { type = "ack", id = cancel.RootElement.GetProperty("id").GetInt64(), ok = true }));
            await cancelling;
            await second.Writer.WriteLineAsync(Result(cancelId, "cancelled"));
            await Until(() => !edit.IsWaiting);
            Require(edit.Confirmed == "same-public-client" && edit.Draft == "new-unsaved-candidate" && !edit.Saved,
                "Cancellation changed the saved ID or lost the draft");

            edit.Begin(edit.Draft, validate: true);
            var finalId = edit.RequestId!;
            await second.Writer.WriteLineAsync(Result(cancelId, "saved"));
            await second.Writer.WriteLineAsync(Result(finalId, "saved"));
            await Until(() => edit.Saved);
            Require(!edit.IsWaiting && edit.Confirmed == "new-unsaved-candidate",
                "A correlated completed test did not apply the saved result");
            second.Engine.Dispose();
            await serving.WaitAsync(IoDeadline);
        }
        return "Real pipe validates operation IDs, rejects early ACK/same-ID snapshots/stale results, replays a failure after reconnect, and cancels without losing the draft";
    }

    private static async Task<string> ActivityMimeMismatchPreservesItemReason()
    {
        const string exactReason = "The new version’s MIME type must match the previous version’s MIME type.";
        var mimeError = "HTTP 400: " + JsonSerializer.Serialize(new
        {
            jsonapi = new { version = "1.0" },
            errors = new[] { new { code = "ERR_WIP_ERROR", title = "VER_MISMATCH_MIMETYPE", detail = exactReason } },
        });
        var permissionError = "HTTP 403: " + JsonSerializer.Serialize(new
        {
            errors = new[] { new { code = "ERR_FORBIDDEN", title = "Permission denied", detail = "Cannot update versions in this folder." } },
        });
        using var pipes = await PipePair.Connect();
        var client = NewClient();
        var serving = Serve(client, pipes.Window);
        await pipes.AckHello();
        foreach (var (name, detail) in new[] { ("drawing.rvt", mimeError), ("sheet.pdf", permissionError) })
        {
            await pipes.Writer.WriteLineAsync(JsonSerializer.Serialize(new
            {
                type = "activity_item",
                item = new { ts_ms = 1L, kind = "error", name, project = "Private audit project", detail },
            }));
        }
        await Until(() => client.Activity.Count == 2);
        var mimeItem = client.Activity.Single(item => item.Name == "drawing.rvt");
        var permissionItem = client.Activity.Single(item => item.Name == "sheet.pdf");
        var mimeDetail = ErrorDisplay.FailureDetail(mimeItem.Detail);
        var permissionDetail = ErrorDisplay.FailureDetail(permissionItem.Detail);
        Require(mimeDetail.Contains("HTTP 400", StringComparison.Ordinal)
            && mimeDetail.Contains("ERR_WIP_ERROR", StringComparison.Ordinal)
            && mimeDetail.Contains("VER_MISMATCH_MIMETYPE", StringComparison.Ordinal)
            && mimeDetail.Contains(exactReason, StringComparison.Ordinal)
            && mimeDetail.Contains("upload it as a new file", StringComparison.Ordinal),
            "The reported MIME mismatch was hidden or lost its specific remedy");
        Require(permissionDetail.Contains("HTTP 403", StringComparison.Ordinal)
            && permissionDetail.Contains("Cannot update versions in this folder.", StringComparison.Ordinal)
            && !permissionDetail.Contains("MIMETYPE", StringComparison.Ordinal)
            && !mimeDetail.Contains("ERR_FORBIDDEN", StringComparison.Ordinal),
            "Different failed files did not retain independent actionable reasons");
        Require(!ErrorDisplay.FriendlyDetail(mimeItem.Detail).Contains("{", StringComparison.Ordinal)
            && !ErrorDisplay.FriendlyDetail(mimeItem.Detail).Contains(exactReason, StringComparison.Ordinal),
            "The compact visible row unexpectedly expanded to a raw response");
        pipes.Engine.Dispose();
        await serving.WaitAsync(IoDeadline);
        return "Two actual activity frames retained separate per-file causes; HTTP400 MIME mismatch includes status, exact reason, code and remedy while the visible summary stays compact";
    }

    private static Task<string> ActivityErrorDetailsRedactCredentials()
    {
        const string privateJwt = "eyJwcml2YXRl.eyJzZWNyZXQ.signature";
        var raw = "HTTP 403: " + JsonSerializer.Serialize(new
        {
            errors = new[]
            {
                new
                {
                    title = "Permission denied", code = "ERR_FORBIDDEN",
                    detail = "Permission to update this file was denied.\n"
                        + "https://storage.example.invalid/file?X-Amz-Signature=PRIVATE_URL\n"
                        + "access_token=PRIVATE_ACCESS; refresh_token='PRIVATE_REFRESH'; client_secret=\"PRIVATE_SECRET\"; password=PRIVATE_PASSWORD; code_verifier=PRIVATE_VERIFIER\n"
                        + "Authorization: Bearer PRIVATE_AUTH\nBasic PRIVATE_BASIC\n"
                        + privateJwt + "\nC:\\Users\\private\\.aws\\credentials",
                    meta = new { password = "PRIVATE_META", access_token = "PRIVATE_META_TOKEN" },
                },
            },
            access_token = "PRIVATE_ROOT", links = new { about = "https://example.invalid/PRIVATE_LINK" },
        });
        var safe = ErrorDisplay.FailureDetail(raw);
        Require(safe.Contains("HTTP 403", StringComparison.Ordinal)
            && safe.Contains("Permission to update this file was denied.", StringComparison.Ordinal)
            && !safe.Contains("PRIVATE_", StringComparison.Ordinal)
            && !safe.Contains(privateJwt, StringComparison.Ordinal)
            && !safe.Contains("https://", StringComparison.OrdinalIgnoreCase)
            && !safe.Contains(".aws", StringComparison.Ordinal)
            && !safe.Contains("\"errors\"", StringComparison.Ordinal),
            "The expanded failure detail exposed a credential, signed URL, local credential path or raw JSON");
        var plain = ErrorDisplay.FriendlyDetail("Permission denied; refresh_token=PRIVATE_PLAIN https://example.invalid/PRIVATE_PLAIN_URL");
        Require(!plain.Contains("PRIVATE_", StringComparison.Ordinal), "Plain visible error text bypassed redaction");
        foreach (var unsafeBody in new[]
        {
            "HTTP 500: {\"errors\":[{\"detail\":\"truncated PRIVATE_TRUNCATED",
            "HTTP 502: <html>PRIVATE_HTML<script>token</script></html>",
            "HTTP 400: {\"access_token\":\"PRIVATE_UNRECOGNIZED\"}",
        })
        {
            var formatted = ErrorDisplay.FailureDetail(unsafeBody);
            Require(!formatted.Contains("PRIVATE_", StringComparison.Ordinal)
                && !formatted.Contains("<html>", StringComparison.Ordinal)
                && formatted.Contains("HTTP ", StringComparison.Ordinal),
                "An unreadable response body escaped the safe fallback");
        }
        Require(ErrorDisplay.FailureDetail("HTTP 500: " + new string('x', 20000)).Length < 1000,
            "An oversized error created an unbounded tooltip");
        return Task.FromResult("Known error fields preserve the reason while redacting tokens, auth headers, signed URLs, JWTs and local paths; malformed/HTML/unknown bodies stay hidden and input is bounded");
    }
    private static Task<string> ActivityLocalPathsWithSpacesAreFullyRedacted()
    {
        var cases = new (string Raw, string Retained)[]
        {
            (@"Reset failed: C:\Users\private-audit\Synka\private-org\Private Project Center", "Reset failed:"),
            ("Cannot open \"C:\\Private Directory\\O'Brien, Jr\\file name.rvt\": Access denied (os error 5).", "Access denied (os error 5)."),
            (@"Cannot move \\private-server\private share\Private Project Center.", "Cannot move"),
            (@"Cannot remove \\?\C:\Private Directory\Private Project Center", "Cannot remove"),
            (@"Cannot remove C:/Private Directory/Private Project Center", "Cannot remove"),
            (@"Cannot open 'C:\Private Directory\O'Brien Folder\file name.rvt': Access denied.", "Cannot open"),
            ("Reset refused:\r\nC:\\Private Root\\Private Project Center\r\nWindows error: Access denied (os error 5).\nClose open files and retry.", "Close open files and retry."),
            ("Cannot move \"C:\\Private Source\\Private Project Center\" -> \"D:\\Private Target\\Private Project Annex\"; close files and retry.", "close files and retry."),
            (@"Cannot move C:\Private Source\Private Project Center -> D:\Private Target\Private Project Annex", "Cannot move"),
            (@"Cannot open C:\Private, Directory; Archive\Private.Project (Draft) [Private Center].", "Cannot open"),
        };
        foreach (var (raw, retained) in cases)
        {
            var jsonError = "HTTP 400: " + JsonSerializer.Serialize(new
            {
                errors = new[] { new { title = "Reset refused", detail = raw } },
            });
            foreach (var safe in new[] { ErrorDisplay.FriendlyDetail(raw), ErrorDisplay.FailureDetail(jsonError) })
            {
                Require(safe.Contains("[local path removed]", StringComparison.Ordinal)
                    && safe.Contains(retained, StringComparison.Ordinal)
                    && !new[] { "Private", "Center", "Annex", "Brien", "file name", "private-" }
                        .Any(part => safe.Contains(part, StringComparison.OrdinalIgnoreCase)),
                    "A local path with spaces, punctuation or adjacent paths exposed a tail or erased separate error context");
            }
        }
        var mixed = "Reset failed:\nC:\\Private Directory\\Private Project Center\n"
            + "access_token=PRIVATE_ACCESS; password=\"PRIVATE PASSWORD\"\n"
            + "Authorization: Bearer PRIVATE_AUTH\n"
            + "https://storage.example.invalid/file?X-Amz-Signature=PRIVATE_URL\n"
            + "Close open files and retry.";
        var sanitized = ErrorDisplay.FriendlyDetail(mixed);
        Require(!sanitized.Contains("PRIVATE", StringComparison.OrdinalIgnoreCase)
            && !sanitized.Contains("Center", StringComparison.Ordinal)
            && sanitized.Contains("Close open files and retry.", StringComparison.Ordinal),
            "Multiline path redaction bypassed credential/URL redaction or erased the recovery action");
        var clock = Stopwatch.StartNew();
        var hostile = string.Concat(Enumerable.Repeat("C:\\Private Directory\\Private Project Center; \n", 1000));
        Require(ErrorDisplay.FriendlyDetail(hostile).Length < 600
            && ErrorDisplay.FailureDetail("HTTP 400: " + hostile).Length < 1000
            && clock.Elapsed < TimeSpan.FromSeconds(5),
            "Repeated hostile paths exceeded bounded rendering limits");
        const string unrelated = "Windows error 5. Close open files and retry.";
        Require(ErrorDisplay.FriendlyDetail(unrelated) == unrelated, "A message without a local path was changed");
        return Task.FromResult("Synthetic screenshot-shaped paths leave no space-separated tails; quoted, UNC, extended, multiline, punctuation and adjacent paths remain private while separate recovery text and credential redaction survive");
    }
    private static Task<string> DisconnectNotifiesSignedInBindings()
    {
        var client = NewClient();
        typeof(EngineClient).GetMethod("HandleLine", PrivateInstance)!.Invoke(client,
            new object[] { "{\"type\":\"status\",\"signed_in\":true,\"text\":\"signed in\"}" });
        var publish = typeof(EngineClient).GetMethod("PublishState", PrivateInstance)!;
        publish.Invoke(client, new object[] { EngineConnectionState.Connected, true });
        Require(client.IsSignedIn, "Signed-in fixture did not become connected");
        var changed = new List<string?>();
        client.PropertyChanged += (_, args) => changed.Add(args.PropertyName);
        publish.Invoke(client, new object[] { EngineConnectionState.Disconnected, false });
        Require(!client.IsSignedIn, "Disconnected client still reports signed in");
        Require(changed.Contains(nameof(EngineClient.IsSignedIn)),
            $"IsSignedIn changed true→false on disconnect but PropertyChanged only raised: {string.Join(',', changed)}");
        return Task.FromResult("Disconnect raised PropertyChanged for the dependent IsSignedIn property");
    }

    private static async Task<string> RapidCommandsHaveABoundedPendingQueue()
    {
        using var pipes = await PipePair.Connect();
        var client = NewClient();
        var serving = Serve(client, pipes.Window);
        await pipes.AckHello();
        var calls = Enumerable.Range(0, 257).Select(_ => client.SyncNowAsync()).ToArray();
        var pending = (System.Collections.IDictionary)typeof(EngineClient).GetField("_pending", PrivateInstance)!.GetValue(client)!;
        var pendingCount = pending.Count;
        var immediateRejections = calls.Count(call => call.IsCompletedSuccessfully && !call.Result.Ok);
        pipes.Engine.Dispose();
        await serving.WaitAsync(IoDeadline);
        var acks = await Task.WhenAll(calls).WaitAsync(IoDeadline);
        Require(acks.All(ack => !ack.Ok), "A command without an engine ACK reported success");
        Require(pendingCount <= 128 && immediateRejections >= 129,
            $"257 rapid commands retained {pendingCount} pending ACKs and immediately rejected only {immediateRejections}; expected at most128 pending and at least129 rejections");
        Require(pending.Count == 0, "Disconnect left pending ACK completions behind");
        return $"257 rapid calls retained {pendingCount} pending requests; {immediateRejections} rejected immediately; disconnect cleared every caller";
    }

    private static async Task<string> OversizedCommandIsRejectedBeforeTransport()
    {
        using var pipes = await PipePair.Connect();
        var client = NewClient();
        var serving = Serve(client, pipes.Window);
        await pipes.AckHello();
        var call = client.SendAsync("set_project", new Dictionary<string, object?>
        {
            ["hub_id"] = "private-fake-hub",
            ["project_id"] = new string('x', 70 * 1024),
            ["subscribe"] = false,
        });
        var answeredPromptly = await Task.WhenAny(call, Task.Delay(250)) == call;
        string? written = null;
        using (var idle = new CancellationTokenSource(TimeSpan.FromMilliseconds(250)))
        {
            try { written = await pipes.Reader.ReadLineAsync(idle.Token); }
            catch (Exception exception) when (exception is OperationCanceledException or IOException) { }
        }
        pipes.Engine.Dispose();
        await serving.WaitAsync(IoDeadline);
        var ack = await call.WaitAsync(IoDeadline);
        Require(answeredPromptly && !ack.Ok && written is null,
            $"A >64KiB command was not rejected before I/O: prompt={answeredPromptly}, ok={ack.Ok}, transmittedChars={written?.Length ?? 0}");
        return "A70KiB project identifier was rejected locally; no bytes reached the engine";
    }
}
