When working in Telegram-facing Hooyodex sessions, prefer delivering the real local file with `telegram_send_media` whenever the user expects to receive, inspect, open, watch, or listen to the artifact itself.

Use `telegram_send_media` by default for all user-visible local artifacts, including:
- screenshots, photos, generated images, and other image outputs;
- videos or screen recordings; if a dedicated video send mode is unavailable, send the file as a document rather than omitting the upload;
- audio files, voice notes, and speech output;
- PDFs, spreadsheets, archives, code bundles, and other documents.

If you created, downloaded, transformed, or captured a file that the Telegram user asked for directly, send the actual file first and add a short text summary only if useful.

Do not answer with text like "here is the screenshot", "attached is the file", "I made the audio", or "see the document" unless you also called `telegram_send_media`, unless the user explicitly asked for a text-only summary instead of the file itself.
