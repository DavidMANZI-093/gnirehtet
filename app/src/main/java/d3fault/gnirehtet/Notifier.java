/*
 * Copyright (C) 2017 Genymobile
 * Copyright (C) 2026 David MANZI
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *     http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

package d3fault.gnirehtet;

import android.app.Notification;
import android.app.NotificationChannel;
import android.app.NotificationManager;
import android.app.PendingIntent;
import android.app.Service;
import android.content.Context;
import android.content.Intent;
import android.graphics.drawable.Icon;
import android.os.Build;

/** Manage the notification necessary for the foreground service (mandatory since Android O). */
public class Notifier {
  private static final int NOTIFICATION_ID = 42;
  private static final String CHANNEL_ID = "Gnirehtet";

  private final Service context;
  private boolean failure;

  public Notifier(Service context) {
    this.context = context;
  }

  private Notification createNotification(boolean failure) {
    Notification.Builder notificationBuilder = createNotificationBuilder();
    notificationBuilder.setContentTitle(context.getString(R.string.app_name));
    if (failure) {
      notificationBuilder.setContentText(context.getString(R.string.relay_disconnected));
      notificationBuilder.setSmallIcon(R.drawable.ic_report_problem_24dp);
    } else {
      notificationBuilder.setContentText(context.getString(R.string.relay_connected));
      notificationBuilder.setSmallIcon(R.drawable.ic_usb_24dp);
    }
    notificationBuilder.addAction(createStopAction());
    return notificationBuilder.build();
  }

  @SuppressWarnings("deprecation")
  private Notification.Action createStopAction() {
    Intent stopIntent = GnirehtetService.createStopIntent(context);
    PendingIntent stopPendingIntent =
        PendingIntent.getService(
            context,
            0,
            stopIntent,
            PendingIntent.FLAG_ONE_SHOT | PendingIntent.FLAG_IMMUTABLE);
    Notification.Action.Builder actionBuilder;
    if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.M) {
      actionBuilder =
          new Notification.Action.Builder(
              Icon.createWithResource(context, R.drawable.ic_close_24dp),
              context.getString(R.string.stop_vpn),
              stopPendingIntent);
    } else {
      actionBuilder =
          new Notification.Action.Builder(
              R.drawable.ic_close_24dp,
              context.getString(R.string.stop_vpn),
              stopPendingIntent);
    }
    return actionBuilder.build();
  }

  @SuppressWarnings("deprecation")
  private Notification.Builder createNotificationBuilder() {
    if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
      return new Notification.Builder(context, CHANNEL_ID);
    }
    // constructor deprecated; solely fallback for earlier APIs
    return new Notification.Builder(context);
  }

  private void createNotificationChannel() {
    NotificationChannel channel =
        new NotificationChannel(
            CHANNEL_ID,
            context.getString(R.string.app_name),
            NotificationManager.IMPORTANCE_DEFAULT);
    getNotificationManager().createNotificationChannel(channel);
  }

  private NotificationManager getNotificationManager() {
    return (NotificationManager) context.getSystemService(Context.NOTIFICATION_SERVICE);
  }

  public void start() {
    failure = false; // reset failure flag
    if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
      createNotificationChannel();
    }
    context.startForeground(NOTIFICATION_ID, createNotification(false));
  }

  @SuppressWarnings("deprecation")
  public void stop() {
    if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.N) {
      context.stopForeground(Service.STOP_FOREGROUND_REMOVE);
    } else {
      context.stopForeground(true);
    }
    if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
      deleteNotificationChannel();
    }
  }

  public void setFailure(boolean failure) {
    if (this.failure != failure) {
      this.failure = failure;
      Notification notification = createNotification(failure);
      getNotificationManager().notify(NOTIFICATION_ID, notification);
    }
  }

  private void deleteNotificationChannel() {
    getNotificationManager().deleteNotificationChannel(CHANNEL_ID);
  }
}
