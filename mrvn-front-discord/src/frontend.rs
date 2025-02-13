use mrvn_back_ytdl::{Brain, Song, EndedHandler, GuildSpeakerEndedHandle};
use mrvn_model::{AppModel, GuildModel, NextEntry, VoteStatus, ReplaceStatus, VoteType};
use std::sync::Arc;
use serenity::{prelude::*, model::prelude::{UserId, GuildId, interactions, application_command}};
use crate::config::Config;
use std::ops::DerefMut;
use crate::message::{send_messages, Message, ResponseMessage, ActionMessage, SendMessageDestination};
use crate::model_delegate::ModelDelegate;
use serenity::model::id::ChannelId;
use std::time::Duration;
use futures::prelude::*;

const SEND_WORKING_TIMEOUT_MS: u64 = 50;

enum HandleCommandError {
    CreateError(crate::error::Error),
    EditError(crate::error::Error),
}

pub struct Frontend {
    pub config: Arc<Config>,
    pub backend_brain: Brain,
    pub model: AppModel<Song>,
}

impl Frontend {
    pub fn new(
        config: Arc<Config>,
        backend_brain: Brain,
        model: AppModel<Song>,
    ) -> Frontend {
        Frontend {
            config,
            backend_brain,
            model,
        }
    }

    pub async fn handle_command(
        self: &Arc<Self>,
        ctx: &Context,
        command: &interactions::application_command::ApplicationCommandInteraction
    ) {
        let send_error_res = match self.handle_command_fallable(ctx, command).await {
            Ok(_) => Ok(()),
            Err(HandleCommandError::CreateError(why)) => {
                log::error!("Error while handling command: {}", why);
                command.create_interaction_response(&ctx.http, |response| {
                    response.kind(interactions::InteractionResponseType::ChannelMessageWithSource)
                        .interaction_response_data(|data| {
                            data.create_embed(|embed| {
                                embed
                                    .description(self.config.get_raw_message("action.unknown_error"))
                                    .color(self.config.embed_color)
                            })
                        })
                }).await.map(|_| ())
            }
            Err(HandleCommandError::EditError(why)) => {
                log::error!("Error while handling command: {}", why);
                command.edit_original_interaction_response(&ctx.http, |response| {
                    response.create_embed(|embed| {
                        embed
                            .description(self.config.get_raw_message("action.unknown_error"))
                            .color(self.config.embed_color)
                    })
                }).await.map(|_| ())
            }
        };

        if let Err(why) = send_error_res {
            log::error!("Error while sending error response: {}", why);
        }
    }

    async fn handle_command_fallable(
        self: &Arc<Self>,
        ctx: &Context,
        command: &interactions::application_command::ApplicationCommandInteraction
    ) -> Result<(), HandleCommandError> {
        let guild_id = command.guild_id.ok_or(HandleCommandError::CreateError(crate::error::Error::NoGuild))?;
        let message_channel_id = command.channel_id;

        // This signal is used to cancel sending a "loading..." message when we finish executing
        // the command.
        let (tx, rx) = tokio::sync::oneshot::channel();
        let send_deferred_message_future = async {
            let show_deferred_message = futures::select!(
                _ = rx.fuse() => false,
                _ = tokio::time::sleep(Duration::from_millis(SEND_WORKING_TIMEOUT_MS)).fuse() => true,
            );
            if show_deferred_message {
                if let Err(why) = command.create_interaction_response(&ctx.http, |response| {
                    response.kind(interactions::InteractionResponseType::DeferredChannelMessageWithSource)
                }).await {
                    log::error!("Error while sending deferred message: {}", why);
                }
            }
        };

        let send_future = async {
            // Ensure we have the guild locked for the duration of the command.
            let guild_model_handle = self.model.get(guild_id);
            let mut guild_model = guild_model_handle.lock().await;
            guild_model.set_message_channel(Some(message_channel_id));

            // Execute the command
            let messages_res = self
                .handle_guild_command(ctx, command, guild_id, guild_model.deref_mut())
                .await;

            // If the timeout has finished, rx will be closed so this send call will return an
            // error. We can use this to know that a response has been created, and we need to edit
            // it from now on.
            let has_sent_deferred = tx.send(()).is_err();
            let messages = messages_res
                .map_err(if has_sent_deferred { HandleCommandError::EditError } else { HandleCommandError::CreateError })?;

            let send_res = send_messages(
                &self.config,
                ctx,
                SendMessageDestination::Interaction {
                    interaction: command,
                    is_edit: has_sent_deferred,
                },
                guild_model.deref_mut(),
                messages,
            ).await;
            if let Err(why) = send_res {
                log::error!("Error while sending response: {}", why);
            }

            Ok(())
        };

        let (send_res, _) = futures::join!(send_future, send_deferred_message_future);
        send_res
    }

    async fn handle_guild_command(
        self: &Arc<Self>,
        ctx: &Context,
        command: &interactions::application_command::ApplicationCommandInteraction,
        guild_id: GuildId,
        guild_model: &mut GuildModel<Song>,
    ) -> Result<Vec<crate::message::Message>, crate::error::Error> {
        let user_id = command.user.id;
        match command.data.name.as_str() {
            "play" => {
                let maybe_term = match command.data.options.get(0).and_then(|val| val.resolved.as_ref()) {
                    Some(application_command::ApplicationCommandInteractionDataOptionValue::String(val)) => Some(val.clone()),
                    _ => None,
                };

                match maybe_term {
                    Some(term) => {
                        log::debug!("Received play, interpreted as queue-play \"{}\"", term);
                        self.handle_queue_play_command(ctx, user_id, guild_id, guild_model, &term).await
                    }
                    None => {
                        log::debug!("Received play, interpreted as unpause");
                        self.handle_unpause_command(ctx, user_id, guild_id, guild_model).await
                    }
                }
            }
            "replace" => {
                let term = match command.data.options.get(0).and_then(|val| val.resolved.as_ref()) {
                    Some(application_command::ApplicationCommandInteractionDataOptionValue::String(val)) => val.clone(),
                    _ => "".to_string(),
                };

                log::debug!("Received replace \"{}\"", term);
                self.handle_replace_command(ctx, user_id, guild_id, guild_model, &term).await
            }
            "pause" => {
                log::debug!("Received pause");
                self.handle_pause_command(ctx, user_id, guild_id).await
            }
            "skip" => {
                log::debug!("Received skip");
                self.handle_skip_command(ctx, user_id, guild_id, guild_model).await
            }
            "stop" => {
                log::debug!("Received stop");
                self.handle_stop_command(ctx, user_id, guild_id, guild_model).await
            }
            command_name => Err(crate::error::Error::UnknownCommand(command_name.to_string())),
        }
    }

    async fn handle_queue_play_command(
        self: &Arc<Self>,
        ctx: &Context,
        user_id: UserId,
        guild_id: GuildId,
        guild_model: &mut GuildModel<Song>,
        term: &str,
    ) -> Result<Vec<crate::message::Message>, crate::error::Error> {
        let delegate_future = ModelDelegate::new(ctx, guild_id);
        let song_future = async {
            Song::load(term, user_id).await.map_err(crate::error::Error::Backend)
        };

        let (delegate, song) = match futures::try_join!(delegate_future, song_future) {
            Ok((delegate, song)) => (delegate, song),
            Err(crate::error::Error::Backend(mrvn_back_ytdl::Error::NoSongsFound)) => {
                return Ok(vec![Message::Response(ResponseMessage::NoMatchingSongsError)]);
            },
            Err(err) => return Err(err),
        };

        let song_metadata = song.metadata.clone();
        log::trace!("Resolved song query as {} (\"{}\")", song_metadata.url, song_metadata.title);

        guild_model.push_entry(user_id, song);

        // From this point on the user needs to be in a channel, otherwise the song will only stay
        // queued.
        let channel_id = match delegate.get_user_voice_channel(user_id) {
            Some(channel) => channel,
            None => {
                log::trace!("User is not in any voice channel, song will remain queued");
                return Ok(vec![Message::Response(ResponseMessage::Queued {
                    song_title: song_metadata.title,
                    song_url: song_metadata.url,
                })])
            },
        };

        // Find a speaker that will be able to play in this channel. We do this before checking if
        // we actually need to play anything so the song can stay in the queue if a speaker isn't
        // found.
        let guild_speakers_handle = self.backend_brain.guild_speakers(guild_id);
        let mut guild_speakers_ref = guild_speakers_handle.lock().await;
        let guild_speaker = match guild_speakers_ref.find_to_play_in_channel(channel_id) {
            Some(speaker) => speaker,
            None => {
                log::trace!("No speakers are available to handle playback, song will remain queued");
                return Ok(vec![Message::Response(ResponseMessage::QueuedNoSpeakers {
                    song_title: song_metadata.title,
                    song_url: song_metadata.url,
                })])
            }
        };

        // Play a song if the model indicates one isn't playing.
        let next_song = match guild_model.next_channel_entry(&delegate, channel_id) {
            NextEntry::Entry(song) => song,
            NextEntry::AlreadyPlaying | NextEntry::NoneAvailable => {
                log::trace!("Channel is already playing, song will remain queued");
                return Ok(vec![Message::Response(ResponseMessage::Queued {
                    song_title: song_metadata.title,
                    song_url: song_metadata.url,
                })])
            }
        };

        let next_metadata = next_song.metadata.clone();
        log::trace!("Playing \"{}\" to speaker", next_metadata.title);
        guild_speaker.play(channel_id, next_song, EndedDelegate {
            frontend: self.clone(),
            ctx: ctx.clone(),
            guild_id,
            channel_id,
        }).await.map_err(crate::error::Error::Backend)?;

        // We could be in one of two states:
        //  - The song that's now playing is the one we just queued, in which case we only show a
        //    "playing" message.
        //  - We queued a song and started a different song, which can happen if there were other
        //    songs waiting but we weren't playing at the time. In this case we show a "queued"
        //    message and a "playing" message.
        if next_metadata.url == song_metadata.url {
            Ok(vec![Message::Action(ActionMessage::PlayingResponse {
                song_title: song_metadata.title,
                song_url: song_metadata.url,
                voice_channel_id: channel_id,
            })])
        } else {
            Ok(vec![
                Message::Response(ResponseMessage::Queued {
                    song_title: song_metadata.title,
                    song_url: song_metadata.url,
                }),
                Message::Action(ActionMessage::Playing {
                    song_title: next_metadata.title,
                    song_url: next_metadata.url,
                    voice_channel_id: channel_id,
                    user_id: next_metadata.user_id,
                })
            ])
        }
    }

    async fn handle_unpause_command(
        self: &Arc<Self>,
        ctx: &Context,
        user_id: UserId,
        guild_id: GuildId,
        guild_model: &mut GuildModel<Song>,
    ) -> Result<Vec<crate::message::Message>, crate::error::Error> {
        let delegate = ModelDelegate::new(ctx, guild_id).await?;
        let channel_id = match delegate.get_user_voice_channel(user_id) {
            Some(channel) => channel,
            None => return Ok(vec![Message::Response(ResponseMessage::NotInVoiceChannelError)])
        };

        // See if there's currently a speaker in this channel to unpause.
        let guild_speakers_handle = self.backend_brain.guild_speakers(guild_id);
        let mut guild_speakers_ref = guild_speakers_handle.lock().await;
        if let Some((guild_speaker, active_metadata)) = guild_speakers_ref.find_active_in_channel(channel_id) {
            return if guild_speaker.is_paused() {
                log::trace!("Found a paused speaker in the user's voice channel, starting playback");
                guild_speaker.unpause().map_err(crate::error::Error::Backend)?;
                Ok(vec![Message::Action(ActionMessage::Playing {
                    song_title: active_metadata.title.clone(),
                    song_url: active_metadata.url.clone(),
                    voice_channel_id: channel_id,
                    user_id: active_metadata.user_id,
                })])
            } else {
                log::trace!("Found an unpaused speaker in the user's voice channel, playback will continue");
                Ok(vec![Message::Response(ResponseMessage::AlreadyPlayingError {
                    voice_channel_id: channel_id,
                })])
            };
        };

        // Otherwise, try starting to play in this channel.
        let guild_speaker = match guild_speakers_ref.find_to_play_in_channel(channel_id) {
            Some(speaker) => speaker,
            None => {
                log::trace!("No speakers are available to handle playback, nothing will be played");
                return Ok(vec![Message::Action(ActionMessage::NoSpeakersError {
                    voice_channel_id: channel_id,
                })])
            },
        };
        let next_song = match guild_model.next_channel_entry(&delegate, channel_id) {
            NextEntry::Entry(song) => song,
            NextEntry::AlreadyPlaying | NextEntry::NoneAvailable => {
                log::trace!("No songs are available to play back in the channel, nothing will be played");
                return Ok(vec![Message::Response(ResponseMessage::NothingIsQueuedError {
                    voice_channel_id: channel_id,
                })])
            }
        };

        let next_metadata = next_song.metadata.clone();
        log::trace!("Playing \"{}\" to speaker", next_metadata.title);
        guild_speaker.play(channel_id, next_song, EndedDelegate {
            frontend: self.clone(),
            ctx: ctx.clone(),
            guild_id,
            channel_id,
        }).await.map_err(crate::error::Error::Backend)?;

        Ok(vec![Message::Action(ActionMessage::Playing {
            song_title: next_metadata.title,
            song_url: next_metadata.url,
            voice_channel_id: channel_id,
            user_id: next_metadata.user_id,
        })])
    }

    async fn handle_replace_command(
        self: &Arc<Self>,
        ctx: &Context,
        user_id: UserId,
        guild_id: GuildId,
        guild_model: &mut GuildModel<Song>,
        term: &str,
    ) -> Result<Vec<crate::message::Message>, crate::error::Error> {
        let delegate_future = ModelDelegate::new(ctx, guild_id);
        let song_future = async {
            Song::load(term, user_id).await.map_err(crate::error::Error::Backend)
        };

        let (delegate, song) = match futures::try_join!(delegate_future, song_future) {
            Ok((delegate, song)) => (delegate, song),
            Err(crate::error::Error::Backend(mrvn_back_ytdl::Error::NoSongsFound)) => {
                return Ok(vec![Message::Response(ResponseMessage::NoMatchingSongsError)]);
            },
            Err(err) => return Err(err),
        };

        let song_metadata = song.metadata.clone();
        log::trace!("Resolved song query as {} (\"{}\")", song_metadata.url, song_metadata.title);

        let maybe_channel_id = delegate.get_user_voice_channel(user_id);
        let channel_id = match guild_model.replace_entry(user_id, maybe_channel_id, song) {
            // If the song was queued, no playback changes are needed so we send a status message
            // and leave it there. But if the model indicated we're replacing the current song,
            // we need to start playing the next song.
            ReplaceStatus::Queued => {
                log::trace!("No songs in queue to replace, song will be queued");
                return Ok(vec![Message::Response(ResponseMessage::Queued {
                    song_title: song_metadata.title,
                    song_url: song_metadata.url,
                })]);
            },
            ReplaceStatus::ReplacedInQueue(old_song) => {
                log::trace!("Latest song in the users queue will be replaced");
                return Ok(vec![Message::Response(ResponseMessage::Replaced {
                    old_song_title: old_song.metadata.title,
                    old_song_url: old_song.metadata.url,
                    new_song_title: song_metadata.title,
                    new_song_url: song_metadata.url,
                })]);
            },
            ReplaceStatus::ReplacedCurrent(channel_id) => channel_id,
        };

        log::trace!("Only song queued by user is currently playing, it will be skipped");

        // We're replacing an already-playing song, so if there's no speaker for this channel
        // something has gone very wrong :(
        let guild_speakers_handle = self.backend_brain.guild_speakers(guild_id);
        let mut guild_speakers_ref = guild_speakers_handle.lock().await;
        let (guild_speaker, playing_metadata) = guild_speakers_ref
            .find_active_in_channel(channel_id)
            .ok_or(crate::error::Error::ModelPlayingSpeakerNotDesync)?;

        // Play a song if the model indicates one isn't playing.
        let next_song = match guild_model.next_channel_entry_finished(&delegate, channel_id) {
            Some(song) => song,
            None => {
                log::trace!("New song is no longer accessible in queue, nothing will play");
                return Ok(vec![Message::Response(ResponseMessage::NothingIsQueuedError {
                    voice_channel_id: channel_id,
                })])
            }
        };

        let next_metadata = next_song.metadata.clone();
        log::trace!("Playing \"{}\" to speaker", next_metadata.title);
        guild_speaker.play(channel_id, next_song, EndedDelegate {
            frontend: self.clone(),
            ctx: ctx.clone(),
            guild_id,
            channel_id,
        }).await.map_err(crate::error::Error::Backend)?;

        // We could be in one of two states:
        //  - The song that's now playing is the one we just queued, in which case we only show a
        //    "playing" message.
        //  - We queued a song and started a different song, which can happen if there were other
        //    songs waiting but we weren't playing at the time. In this case we show a "queued"
        //    message and a "playing" message.
        if next_metadata.url == song_metadata.url {
            Ok(vec![Message::Action(ActionMessage::PlayingResponse {
                song_title: song_metadata.title,
                song_url: song_metadata.url,
                voice_channel_id: channel_id,
            })])
        } else {
            Ok(vec![
                Message::Response(ResponseMessage::ReplaceSkipped {
                    new_song_title: song_metadata.title,
                    new_song_url: song_metadata.url,
                    old_song_title: playing_metadata.title,
                    old_song_url: playing_metadata.url,
                    voice_channel_id: channel_id,
                }),
                Message::Action(ActionMessage::Playing {
                    song_title: next_metadata.title,
                    song_url: next_metadata.url,
                    voice_channel_id: channel_id,
                    user_id: next_metadata.user_id,
                })
            ])
        }
    }

    async fn handle_pause_command(
        self: &Arc<Self>,
        ctx: &Context,
        user_id: UserId,
        guild_id: GuildId,
    ) -> Result<Vec<crate::message::Message>, crate::error::Error> {
        let delegate = ModelDelegate::new(ctx, guild_id).await?;
        let channel_id = match delegate.get_user_voice_channel(user_id) {
            Some(channel) => channel,
            None => return Ok(vec![Message::Response(ResponseMessage::NotInVoiceChannelError)])
        };

        let guild_speakers_handle = self.backend_brain.guild_speakers(guild_id);
        let mut guild_speakers_ref = guild_speakers_handle.lock().await;
        match guild_speakers_ref.find_active_in_channel(channel_id) {
            Some((guild_speaker, active_metadata)) => {
                if guild_speaker.is_paused() {
                    log::trace!("Found a paused speaker in the user's voice channel, playback will remain paused");
                    Ok(vec![Message::Response(ResponseMessage::NothingIsPlayingError {
                        voice_channel_id: channel_id,
                    })])
                } else {
                    log::trace!("Found an unpaused speaker in the user's voice channel, playback will be paused");
                    guild_speaker.pause().map_err(crate::error::Error::Backend)?;
                    Ok(vec![Message::Response(ResponseMessage::Paused {
                        song_title: active_metadata.title.clone(),
                        song_url: active_metadata.url.clone(),
                        voice_channel_id: channel_id,
                        user_id: active_metadata.user_id,
                    })])
                }
            },
            _ => {
                log::trace!("No speakers are in the user's voice channel, playback will not change");
                Ok(vec![Message::Response(ResponseMessage::NothingIsPlayingError {
                    voice_channel_id: channel_id,
                })])
            }
        }
    }

    async fn handle_skip_command(
        self: &Arc<Self>,
        ctx: &Context,
        user_id: UserId,
        guild_id: GuildId,
        guild_model: &mut GuildModel<Song>,
    ) -> Result<Vec<crate::message::Message>, crate::error::Error> {
        let delegate = ModelDelegate::new(&ctx, guild_id).await?;
        let channel_id = match delegate.get_user_voice_channel(user_id) {
            Some(channel) => channel,
            None => return Ok(vec![Message::Response(ResponseMessage::NotInVoiceChannelError)])
        };

        let skip_status = guild_model.vote_for_skip(&delegate, VoteType::Skip, channel_id, user_id);

        let guild_speakers_handle = self.backend_brain.guild_speakers(guild_id);
        let mut guild_speakers_ref = guild_speakers_handle.lock().await;
        let maybe_guild_speaker = guild_speakers_ref.find_active_in_channel(channel_id);

        match (skip_status, maybe_guild_speaker) {
            (VoteStatus::Success, Some((guild_speaker, active_metadata))) => {
                log::trace!("Skip command passed preconditions, stopping current playback");
                guild_speaker.stop().map_err(crate::error::Error::Backend)?;
                Ok(vec![Message::Response(ResponseMessage::Skipped {
                    song_title: active_metadata.title.clone(),
                    song_url: active_metadata.url.clone(),
                    voice_channel_id: channel_id,
                    user_id: active_metadata.user_id,
                })])
            }
            (VoteStatus::AlreadyVoted, Some((_, active_metadata))) => {
                log::trace!("User attempting to skip has already voted, not stopping playback");
                Ok(vec![Message::Response(ResponseMessage::SkipAlreadyVotedError {
                    song_title: active_metadata.title.clone(),
                    song_url: active_metadata.url.clone(),
                    voice_channel_id: channel_id,
                })])
            }
            (VoteStatus::NeedsMoreVotes(count), Some((_, active_metadata))) => {
                log::trace!("Skip vote has been counted but more are needed, not stopping playback");
                Ok(vec![Message::Response(ResponseMessage::SkipMoreVotesNeeded {
                    song_title: active_metadata.title.clone(),
                    song_url: active_metadata.url.clone(),
                    voice_channel_id: channel_id,
                    count,
                })])
            }
            (VoteStatus::NothingPlaying, _) => {
                log::trace!("Nothing is playing in the user's voice channel, not stopping playback");
                Ok(vec![Message::Response(ResponseMessage::NothingIsPlayingError {
                    voice_channel_id: channel_id,
                })])
            }
            (_, None) => Err(crate::error::Error::ModelPlayingSpeakerNotDesync)
        }
    }

    async fn handle_stop_command(
        self: &Arc<Self>,
        ctx: &Context,
        user_id: UserId,
        guild_id: GuildId,
        guild_model: &mut GuildModel<Song>,
    ) -> Result<Vec<crate::message::Message>, crate::error::Error> {
        let delegate = ModelDelegate::new(&ctx, guild_id).await?;
        let channel_id = match delegate.get_user_voice_channel(user_id) {
            Some(channel) => channel,
            None => return Ok(vec![Message::Response(ResponseMessage::NotInVoiceChannelError)])
        };

        match guild_model.vote_for_skip(&delegate, VoteType::Stop, channel_id, user_id) {
            VoteStatus::Success => {
                let guild_speakers_handle = self.backend_brain.guild_speakers(guild_id);
                let mut guild_speakers_ref = guild_speakers_handle.lock().await;
                let maybe_guild_speaker = guild_speakers_ref.find_active_in_channel(channel_id);
                match maybe_guild_speaker {
                    Some((guild_speaker, active_metadata)) => {
                        log::trace!("Stop command passed preconditions, stopping playback");
                        guild_model.set_channel_stopped(channel_id);
                        guild_speaker.stop().map_err(crate::error::Error::Backend)?;
                        Ok(vec![Message::Response(ResponseMessage::Stopped {
                            song_title: active_metadata.title.clone(),
                            song_url: active_metadata.url.clone(),
                            voice_channel_id: channel_id,
                            user_id: active_metadata.user_id,
                        })])
                    }
                    None => Err(crate::error::Error::ModelPlayingSpeakerNotDesync)
                }
            }
            VoteStatus::AlreadyVoted => {
                log::trace!("User attempting to stop has already voted, not stopping playback");
                Ok(vec![Message::Response(ResponseMessage::StopAlreadyVotedError {
                    voice_channel_id: channel_id,
                })])
            }
            VoteStatus::NeedsMoreVotes(count) => {
                log::trace!("Stop vote has been counted but more are needed, not stopping playback");
                Ok(vec![Message::Response(ResponseMessage::StopMoreVotesNeeded {
                    voice_channel_id: channel_id,
                    count,
                })])
            }
            VoteStatus::NothingPlaying => {
                log::trace!("Nothing is playing in the user's voice channel, not stopping playback");
                Ok(vec![Message::Response(ResponseMessage::NothingIsPlayingError {
                    voice_channel_id: channel_id,
                })])
            }
        }
    }

    async fn handle_playback_ended(self: Arc<Self>, ctx: Context, guild_id: GuildId, channel_id: ChannelId, ended_handle: GuildSpeakerEndedHandle) {
        log::trace!("Playback has ended, preparing to play the next available song");

        let guild_model_handle = self.model.get(guild_id);
        let mut guild_model = guild_model_handle.lock().await;

        let maybe_message_channel = guild_model.message_channel();
        let messages = self.continue_channel_playback(&ctx, guild_id, guild_model.deref_mut(), channel_id, ended_handle).await;
        let send_result = match (messages, maybe_message_channel) {
            (Ok(messages), Some(message_channel)) => {
                send_messages(&self.config, &ctx, SendMessageDestination::Channel(message_channel), guild_model.deref_mut(), messages).await
            },
            (Err(why), Some(message_channel)) => {
                log::error!("Error while continuing playback: {}", why);
                send_messages(&self.config, &ctx, SendMessageDestination::Channel(message_channel), guild_model.deref_mut(), vec![
                    Message::Action(ActionMessage::UnknownError)
                ]).await
            },
            (Err(why), _) => Err(why),
            (_, None) => Ok(()),
        };

        if let Err(why) = send_result {
            log::error!("Error while continuing playback: {}", why);
        }
    }

    async fn continue_channel_playback(
        self: &Arc<Self>,
        ctx: &Context,
        guild_id: GuildId,
        guild_model: &mut GuildModel<Song>,
        channel_id: ChannelId,
        ended_handle: GuildSpeakerEndedHandle,
    ) -> Result<Vec<crate::message::Message>, crate::error::Error> {
        if guild_model.is_channel_stopped(channel_id) {
            log::trace!("Channel has been stopped, not playing any more songs.");
            ended_handle.stop().await;
            return Ok(Vec::new());
        }

        let delegate = ModelDelegate::new(&ctx, guild_id).await?;
        match guild_model.next_channel_entry_finished(&delegate, channel_id) {
            Some(song) => {
                let next_metadata = song.metadata.clone();
                log::trace!("Playing \"{}\" to speaker", next_metadata.title);
                ended_handle.play(channel_id, song, EndedDelegate {
                    frontend: self.clone(),
                    ctx: ctx.clone(),
                    guild_id,
                    channel_id,
                }).await.map_err(crate::error::Error::Backend)?;

                Ok(vec![Message::Action(ActionMessage::Playing {
                    song_title: next_metadata.title,
                    song_url: next_metadata.url,
                    voice_channel_id: channel_id,
                    user_id: next_metadata.user_id,
                })])
            }
            None => {
                log::trace!("No songs are available to play in the channel, nothing will be played");

                ended_handle.stop().await;
                return Ok(vec![Message::Action(ActionMessage::Finished {
                    voice_channel_id: channel_id,
                })])
            }
        }
    }
}

struct EndedDelegate {
    frontend: Arc<Frontend>,
    ctx: Context,
    guild_id: GuildId,
    channel_id: ChannelId,
}

impl EndedHandler for EndedDelegate {
    fn on_ended(self, ended_handle: GuildSpeakerEndedHandle) {
        tokio::task::spawn(self.frontend.handle_playback_ended(self.ctx, self.guild_id, self.channel_id, ended_handle));
    }
}
