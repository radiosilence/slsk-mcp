-- The jobs still in hand, newest first, for the job list drawn every second:
-- without it that list walks every job ever made to find the few in hand.
CREATE INDEX jobs_active ON jobs (created_at DESC) WHERE status NOT IN ('imported', 'cancelled');
