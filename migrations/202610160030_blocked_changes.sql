-- Wall flips get their own change classes instead of renames to null.
ALTER TABLE resource_change DROP CONSTRAINT resource_change_class_check;
ALTER TABLE resource_change ADD CONSTRAINT resource_change_class_check CHECK (class IN
 ('created','rescheduled','relocated','cancelled','uncancelled','renamed','details','deleted','blocked','recovered'));
