// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

package dev.lapoule.tuile;

import android.app.AlertDialog;
import android.app.NativeActivity;
import android.content.ClipData;
import android.content.ClipboardManager;
import android.content.Context;
import android.content.Intent;
import android.graphics.Color;
import android.os.Build;
import android.os.Bundle;
import android.os.PowerManager;
import android.util.TypedValue;
import android.view.Gravity;
import android.view.View;
import android.view.ViewGroup;
import android.widget.PopupWindow;
import android.widget.TextView;

/**
 * The viewer's activity: the platform's own native activity, which hands the
 * window, its input and its lifecycle to the Rust application, plus the few
 * things a native thread cannot do by itself on this system.
 *
 * <ul>
 *   <li>Memory pressure arrives as {@code onTrimMemory}, which the native
 *       activity does not forward. It is counted here, and the application
 *       reads the count and the level once a second — no native method is
 *       registered, so no symbol is exported for it.
 *   <li>Views (the caption, an alert) belong to the UI thread: the
 *       application calls these methods from its own thread and they post the
 *       work there.
 *   <li>The temperature, the battery saver and the launch's settings are read
 *       through the system's services.
 * </ul>
 *
 * Nothing here decides anything: every policy — what a trim level means, what
 * to let go of, which launch settings to accept — is in Rust, and tested there.
 */
public class TuileActivity extends NativeActivity {
    /** How many times the system has said it is short of memory, and the last level it said. */
    private static volatile int trimCount = 0;
    private static volatile int trimLevel = 0;

    private PopupWindow caption;
    private TextView captionText;
    /** The last caption asked for: shown once the window can carry it. UI thread only. */
    private String captionWanted = "";
    private boolean captionRetrying = false;

    @Override
    public void onTrimMemory(int level) {
        super.onTrimMemory(level);
        trimLevel = level;
        trimCount = trimCount + 1;
    }

    /** The last trim level the system sent, in the low 32 bits; how many it has sent, above. */
    public long trimSignal() {
        return ((long) trimCount << 32) | (trimLevel & 0xffffffffL);
    }

    /** PowerManager's thermal status, 0 (none) to 6 (shutdown); 0 before Android 10. */
    public int thermalStatus() {
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.Q) {
            return 0;
        }
        PowerManager power = (PowerManager) getSystemService(Context.POWER_SERVICE);
        return power == null ? 0 : power.getCurrentThermalStatus();
    }

    /** Whether the battery saver is on. */
    public boolean powerSave() {
        PowerManager power = (PowerManager) getSystemService(Context.POWER_SERVICE);
        return power != null && power.isPowerSaveMode();
    }

    /** The application's cache directory, which the system may clear. */
    public String cacheDirectory() {
        return getCacheDir().getAbsolutePath();
    }

    /** The application's private files directory. */
    public String filesDirectory() {
        return getFilesDir().getAbsolutePath();
    }

    /**
     * The launch intent's string extras, one {@code KEY=VALUE} a line. Which of
     * them the application accepts is decided in Rust.
     */
    public String launchSettings() {
        Intent intent = getIntent();
        Bundle extras = intent == null ? null : intent.getExtras();
        if (extras == null) {
            return "";
        }
        StringBuilder lines = new StringBuilder();
        for (String key : extras.keySet()) {
            Object value = extras.get(key);
            if (value instanceof String && ((String) value).indexOf('\n') < 0) {
                lines.append(key).append('=').append((String) value).append('\n');
            }
        }
        return lines.toString();
    }

    /** A line of text over the bottom of the view: the attribution and the last notice. */
    public void caption(final String text) {
        runOnUiThread(new Runnable() {
            @Override
            public void run() {
                showCaption(text);
            }
        });
    }

    private void showCaption(String text) {
        captionWanted = text;
        final View anchor = getWindow().getDecorView();
        if (anchor.getWindowToken() == null) {
            // Asked before the window is attached — the attribution is set
            // once, at launch: tried again shortly, with whatever is wanted
            // by then. One retry in flight at a time.
            if (!captionRetrying) {
                captionRetrying = true;
                anchor.postDelayed(new Runnable() {
                    @Override
                    public void run() {
                        captionRetrying = false;
                        if (!isFinishing()) {
                            showCaption(captionWanted);
                        }
                    }
                }, 250);
            }
            return;
        }
        if (caption == null) {
            captionText = new TextView(this);
            captionText.setTextColor(Color.WHITE);
            captionText.setShadowLayer(3f, 0f, 1f, Color.BLACK);
            captionText.setTextSize(TypedValue.COMPLEX_UNIT_SP, 11f);
            captionText.setGravity(Gravity.CENTER);
            int pad = (int) TypedValue.applyDimension(
                    TypedValue.COMPLEX_UNIT_DIP, 6f, getResources().getDisplayMetrics());
            captionText.setPadding(pad, pad, pad, pad);
            caption = new PopupWindow(captionText,
                    ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.WRAP_CONTENT);
            // The globe keeps every touch: the caption is only drawn over it.
            caption.setTouchable(false);
            caption.setFocusable(false);
            caption.setClippingEnabled(true);
        }
        captionText.setText(text);
        if (text.isEmpty()) {
            caption.dismiss();
        } else if (!caption.isShowing()) {
            caption.showAtLocation(anchor, Gravity.BOTTOM | Gravity.CENTER_HORIZONTAL, 0, 0);
        }
    }

    @Override
    protected void onDestroy() {
        if (caption != null) {
            caption.dismiss();
        }
        super.onDestroy();
    }

    /**
     * A message over the view, with one button. Returns whether it can be
     * shown now: not while the activity is finishing or has no window yet.
     */
    public boolean alert(final String title, final String text) {
        if (isFinishing() || getWindow().getDecorView().getWindowToken() == null) {
            return false;
        }
        runOnUiThread(new Runnable() {
            @Override
            public void run() {
                if (isFinishing()) {
                    return;
                }
                new AlertDialog.Builder(TuileActivity.this)
                        .setTitle(title)
                        .setMessage(text)
                        .setPositiveButton(android.R.string.ok, null)
                        .show();
            }
        });
        return true;
    }

    /** Puts a line of text on the clipboard. */
    public boolean copy(String text) {
        ClipboardManager clipboard = (ClipboardManager) getSystemService(Context.CLIPBOARD_SERVICE);
        if (clipboard == null) {
            return false;
        }
        clipboard.setPrimaryClip(ClipData.newPlainText("view", text));
        return true;
    }

    /** The text on the clipboard, or null. */
    public String paste() {
        ClipboardManager clipboard = (ClipboardManager) getSystemService(Context.CLIPBOARD_SERVICE);
        if (clipboard == null || !clipboard.hasPrimaryClip()) {
            return null;
        }
        ClipData clip = clipboard.getPrimaryClip();
        if (clip == null || clip.getItemCount() == 0) {
            return null;
        }
        CharSequence text = clip.getItemAt(0).coerceToText(this);
        return text == null ? null : text.toString();
    }
}
