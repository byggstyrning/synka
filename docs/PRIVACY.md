# Synka privacy policy

Last updated: 3 November 2026

Synka is a Windows desktop app published by Byggstyrning (https://byggstyrning.se). It syncs
construction documents from Autodesk Construction Cloud (ACC Docs), StreamBIM and Interaxo to
an ordinary folder in Windows Explorer. This policy describes what data Synka handles, where it
goes and how you remove it.

**In short:** Synka runs entirely on your PC. Your files and account data move only between your
PC and the platform they belong to. Byggstyrning receives nothing: Synka has no telemetry, no
analytics, no crash reporting and no advertising.

## What Synka handles, and where it is kept

| Data | Why | Where it is kept |
|---|---|---|
| Your account identity on each platform (user ID, name, e-mail address) | To show who is signed in and to label your own changes | On your PC, in Synka's app data folder |
| Sign-in tokens | To stay signed in without asking for your password again | On your PC. Long-lived tokens are encrypted with Windows DPAPI for your Windows user only; short-lived access tokens are kept in memory only |
| Project, folder and file metadata (names, IDs, versions, sizes, dates, who last changed a file, lock state) | To show your projects in Explorer and to know what changed | On your PC, in one local database file |
| File contents | The files you sync | In your sync folder, which you choose (`%USERPROFILE%\Synka` by default) |
| Settings | Your choices in Synka | On your PC, in `settings.json` |
| Logs (technical events and errors, never file contents) | Troubleshooting | On your PC; one file per day, the oldest is deleted after seven files |

Synka never stores the password you use with Autodesk or Interaxo; you sign in on the platform's
own web page. A StreamBIM password is sent once to StreamBIM to sign you in and is never stored
or logged.

## Who receives data

Synka sends data only to the platform that the data belongs to:

- **Autodesk** (Autodesk Platform Services endpoints and Autodesk's file storage) for ACC
  projects. Autodesk's own [privacy statement](https://www.autodesk.com/company/legal-notices-trademarks/privacy-statement)
  applies to that data.
- **StreamBIM** (the servers of the region you sign in to, and the storage links they hand out)
  for StreamBIM projects.
- **Interaxo** (its API hosts) for Interaxo rooms.

Synka does not send data to Byggstyrning or to any other third party. Synka does not check for
updates unless your organisation has turned that on.

Synka asks Autodesk only for the access it needs: reading and writing project files and reading
your basic profile (`data:read data:write data:create user-profile:read`). Your existing ACC
project permissions still decide what you can see and change. An ACC account administrator must
approve Synka for each ACC account before it can reach that account's projects.

## How long data is kept, and how to remove it

- **Sign out** in Synka (the Konton page) to delete the stored tokens and account identity for
  that account from your PC. For Interaxo, Synka also asks Interaxo to revoke the token.
- **Uninstall** Synka to delete its app data folder (database, settings, tokens and logs).
  Files in your sync folder are your own files and stay where they are until you delete them.
- **Revoke access** at any time in your Autodesk account settings (https://accounts.autodesk.com),
  or ask your ACC account administrator to remove Synka from the account.

Because Byggstyrning never receives your data, we hold nothing about you that we could export or
delete. If you have a question or a request, write to us and we will help.

## Children

Synka is a professional tool for the construction industry and is not directed to anyone under 18.

## Changes

We update this policy when Synka's data handling changes. The date at the top shows the latest
version.

## Contact

Byggstyrning, https://byggstyrning.se, info@byggstyrning.se
