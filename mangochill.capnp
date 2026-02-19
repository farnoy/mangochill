@0x8107c07e1d144b0a;

struct Timestamp {
  seconds @0 :Int64;
  microseconds @1 :UInt32;
}

interface PollReceiver {
  receive @0 (collected_at :Timestamp, readings :List(Timestamp));
}

# destroy this handle to unsubscribe
interface Subscription {}

interface RawEvents {
  register @0 (receiver :PollReceiver) -> (subscription :Subscription);
}

interface FpsReceiver {
  receive @0 (fpsLimit :Float32);
}

interface FpsLimiter {
  register @0 (
    frequencyHz :UInt16,
    minFps :UInt16,
    maxFps :UInt16,
    attackHalfLifeMicroseconds :UInt32,
    releaseHalfLifeMicroseconds :UInt32,
    receiver :FpsReceiver
  ) -> (subscription :Subscription);
}

interface MangoChill {
  fpsLimiter @0 () -> (service :FpsLimiter);
  rawEvents @1 () -> (service :RawEvents);
}
