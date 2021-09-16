use arccstr::ArcCStr;

use bytes::BytesMut;
use chrono::{self, NaiveDate, NaiveDateTime, NaiveTime};
use derive_more::{From, Into};
use itertools::Either;
use serde::{Deserialize, Serialize};
use tokio_postgres::types::{accepts, to_sql_checked, IsNull, ToSql, Type};

use crate::{internal, ReadySetError, ReadySetResult};
use nom_sql::{Double, Float, Literal, SqlType};

use std::convert::{TryFrom, TryInto};
use std::error::Error;
use std::hash::{Hash, Hasher};
use std::ops::{Add, Div, Mul, Sub};
use std::{borrow::Cow, mem};
use std::{fmt, iter};

use proptest::prelude::{prop_oneof, Arbitrary};

const TINYTEXT_WIDTH: usize = 15;

/// The main type used for user data throughout the codebase.
///
/// Having this be an enum allows for our code to be agnostic about the types of user data except
/// when type information is specifically necessary.
///
/// Note that cloning a `DataType` using the `Clone` trait is possible, but may result in cache
/// contention on the reference counts for de-duplicated strings. Use `DataType::deep_clone` to
/// clone the *value* of a `DataType` without danger of contention.
///
/// Also note that DataType uses a custom implementation of the Serialize trait, which must be
/// manually updated if the DataType enum changes:
/// https://www.notion.so/Text-TinyText-Serialization-Deserialization-9dff56b6974b4bcdae28f236882783a8
#[derive(Clone)]
#[warn(variant_size_differences)]
pub enum DataType {
    /// An empty value.
    None,
    /// A signed 32-bit numeric value.
    Int(i32),
    /// An unsigned 32-bit numeric value.
    UnsignedInt(u32),
    /// A signed 64-bit numeric value.
    BigInt(i64),
    /// An unsigned signed 64-bit numeric value.
    UnsignedBigInt(u64),
    /// A floating point 32-bit real value. The second field holds the input precision, which is useful for
    /// supporting DECIMAL, as well as characteristics of how FLOAT and DOUBLE behave in MySQL.
    Float(f32, u8),
    /// A floating point 64-bit real value. The second field holds the input precision, which is useful for
    /// supporting DECIMAL, as well as characteristics of how FLOAT and DOUBLE behave in MySQL.
    Double(f64, u8),
    /// A reference-counted string-like value.
    Text(ArcCStr),
    /// A tiny string that fits in a pointer
    TinyText([u8; TINYTEXT_WIDTH]),
    /// A timestamp for date/time types.
    Timestamp(NaiveDateTime),
    /// A time duration
    Time(Arc<MysqlTime>), //NOTE(Fran): Using an `Arc` to keep the `DataType` type 16 bytes long
}

impl Eq for DataType {}

impl fmt::Display for DataType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            DataType::None => write!(f, "NULL"),
            DataType::Text(..) | DataType::TinyText(..) => {
                let text: &str = <&str>::try_from(self).map_err(|_| fmt::Error)?;
                write!(f, "{}", text)
            }
            DataType::Int(n) => write!(f, "{}", n),
            DataType::UnsignedInt(n) => write!(f, "{}", n),
            DataType::BigInt(n) => write!(f, "{}", n),
            DataType::UnsignedBigInt(n) => write!(f, "{}", n),
            DataType::Float(n, _) => write!(f, "{}", n),
            DataType::Double(n, _) => write!(f, "{}", n),
            DataType::Timestamp(ts) => write!(f, "{}", ts.format("%c")),
            DataType::Time(ref t) => {
                write!(f, "{}", t.to_string())
            }
        }
    }
}

impl fmt::Debug for DataType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            DataType::None => write!(f, "None"),
            DataType::Text(..) => {
                let text: &str = <&str>::try_from(self).map_err(|_| fmt::Error)?;
                write!(f, "Text({:?})", text)
            }
            DataType::TinyText(..) => {
                let text: &str = <&str>::try_from(self).map_err(|_| fmt::Error)?;
                write!(f, "TinyText({:?})", text)
            }
            DataType::Timestamp(ts) => write!(f, "Timestamp({:?})", ts),
            DataType::Float(real, p) => write!(f, "Float({}, {})", real, p),
            DataType::Double(real, p) => write!(f, "Double({}, {})", real, p),
            DataType::Int(n) => write!(f, "Int({})", n),
            DataType::UnsignedInt(n) => write!(f, "UnsignedInt({})", n),
            DataType::BigInt(n) => write!(f, "BigInt({})", n),
            DataType::UnsignedBigInt(n) => write!(f, "UnsignedBigInt({})", n),
            DataType::Time(ref t) => f.debug_tuple("Time").field(t.as_ref()).finish(),
        }
    }
}

/// The format for timestamps when parsed as text
pub const TIMESTAMP_FORMAT: &str = "%Y-%m-%d %H:%M:%S";

/// The format for times when parsed as text
pub const TIME_FORMAT: &str = "%H:%M:%S";

/// The format for dates when parsed as text
pub const DATE_FORMAT: &str = "%Y-%m-%d";

impl DataType {
    /// Generates the minimum DataType corresponding to the type of a given DataType.
    pub fn min_value(other: &Self) -> Self {
        match other {
            DataType::None => DataType::None,
            DataType::Text(_) | DataType::TinyText(_) => DataType::TinyText([0; 15]),
            DataType::Timestamp(_) => DataType::Timestamp(NaiveDateTime::new(
                chrono::naive::MIN_DATE,
                NaiveTime::from_hms(0, 0, 0),
            )),
            DataType::Float(..) => DataType::Float(f32::MIN, u8::MAX),
            DataType::Double(..) => DataType::Double(f64::MIN, u8::MAX),
            DataType::Int(_) => DataType::Int(i32::min_value()),
            DataType::UnsignedInt(_) => DataType::UnsignedInt(0),
            DataType::BigInt(_) => DataType::BigInt(i64::min_value()),
            DataType::UnsignedBigInt(_) => DataType::UnsignedInt(0),
            DataType::Time(_) => DataType::Time(Arc::new(MysqlTime::min_value())),
        }
    }

    /// Generates the maximum DataType corresponding to the type of a given DataType.
    /// Note that there is no possible maximum for the `Text` variant, hence it is not implemented.
    pub fn max_value(other: &Self) -> Self {
        match other {
            DataType::None => DataType::None,
            DataType::Text(_) => unimplemented!(),
            DataType::TinyText(_) => DataType::TinyText([u8::max_value(); 15]),
            DataType::Timestamp(_) => DataType::Timestamp(NaiveDateTime::new(
                chrono::naive::MAX_DATE,
                NaiveTime::from_hms(23, 59, 59),
            )),
            DataType::Float(..) => DataType::Float(f32::MAX, u8::MAX),
            DataType::Double(..) => DataType::Double(f64::MIN, u8::MAX),
            DataType::Int(_) => DataType::Int(i32::max_value()),
            DataType::UnsignedInt(_) => DataType::UnsignedInt(u32::max_value()),
            DataType::BigInt(_) => DataType::BigInt(i64::max_value()),
            DataType::UnsignedBigInt(_) => DataType::UnsignedBigInt(u64::max_value()),
            DataType::Time(_) => DataType::Time(Arc::new(MysqlTime::max_value())),
        }
    }

    /// Clone the value contained within this `DataType`.
    ///
    /// This method crucially does not cause cache-line conflicts with the underlying data-store
    /// (i.e., the owner of `self`), at the cost of requiring additional allocation and copying.
    pub fn deep_clone(&self) -> Self {
        match *self {
            DataType::Text(ref cstr) => DataType::Text(ArcCStr::from(&**cstr)),
            ref dt => dt.clone(),
        }
    }

    /// Checks if this value is `DataType::None`.
    pub fn is_none(&self) -> bool {
        matches!(*self, DataType::None)
    }

    /// Checks if this value is of an integral data type (i.e., can be converted into integral types).
    pub fn is_integer(&self) -> bool {
        matches!(*self, DataType::Int(_) | DataType::BigInt(_))
    }

    /// Checks if this value is of a real data type (i.e., can be converted into `f32` or `f64`).
    pub fn is_real(&self) -> bool {
        matches!(*self, DataType::Float(_, _) | DataType::Double(_, _))
    }

    /// Checks if this value is of a string data type (i.e., can be converted into `String` and
    /// `&str`).
    pub fn is_string(&self) -> bool {
        matches!(*self, DataType::Text(_) | DataType::TinyText(_))
    }

    /// Checks if this value is of a timestamp data type.
    pub fn is_datetime(&self) -> bool {
        matches!(*self, DataType::Timestamp(_))
    }

    /// Checks if this value is of a time data type.
    pub fn is_time(&self) -> bool {
        matches!(*self, DataType::Time(_))
    }

    /// Returns true if this datatype is truthy (is not 0, 0.0, '', or NULL)
    ///
    /// # Examples
    ///
    /// ```rust
    /// use noria::DataType;
    ///
    /// assert!(!DataType::None.is_truthy());
    /// assert!(!DataType::Int(0).is_truthy());
    /// assert!(DataType::Int(1).is_truthy());
    /// ```
    pub fn is_truthy(&self) -> bool {
        match *self {
            DataType::None => false,
            DataType::Int(x) => x != 0,
            DataType::UnsignedInt(x) => x != 0,
            DataType::BigInt(x) => x != 0,
            DataType::UnsignedBigInt(x) => x != 0,
            DataType::Float(f, _) => f != 0.0,
            DataType::Double(f, _) => f != 0.0,
            DataType::Text(_) | DataType::TinyText(_) => {
                // Use of TryFrom for these DataTypes always yields an Ok(..) result.
                #[allow(clippy::unwrap_used)]
                !<&str>::try_from(self).unwrap().is_empty()
            }
            DataType::Timestamp(ref dt) => *dt != NaiveDate::from_ymd(0, 0, 0).and_hms(0, 0, 0),
            DataType::Time(ref t) => **t != MysqlTime::from_microseconds(0),
        }
    }

    /// Checks if the given DataType::Double or DataType::Float is equal to another DataType::Double or DataType::Float (respectively)
    /// under an acceptable error margin. If None is supplied, we use f32::EPSILON or f64::EPSILON, accordingly.
    pub fn equal_under_error_margin(&self, other: &DataType, error_margin: Option<f64>) -> bool {
        macro_rules! numeric_comparison {
            ($self_num:expr, $other_num:expr, $error_margin:expr, $epsilon:expr) => {
                // Handle NaN and infinite case.
                if $self_num.is_nan()
                    || $other_num.is_nan()
                    || $self_num.is_infinite()
                    || $other_num.is_infinite()
                {
                    // If either self or other is either NaN or some version of infinite,
                    // compare bitwise instead.
                    $self_num.to_bits() == $other_num.to_bits()
                } else {
                    // Now that we've validated that both floats are valid numbers, and
                    // finite, we can compare within a margin of error.
                    ($self_num - $other_num).abs() < $error_margin.unwrap_or($epsilon)
                }
            };
        }
        match self {
            DataType::Float(self_float, _) => match other {
                DataType::Float(other_float, _) => {
                    let other_epsilon = error_margin.map(|f| f as f32);
                    numeric_comparison!(self_float, other_float, other_epsilon, f32::EPSILON)
                }
                _ => false,
            },
            DataType::Double(self_double, _) => match other {
                DataType::Double(other_double, _) => {
                    numeric_comparison!(self_double, other_double, error_margin, f64::EPSILON)
                }
                _ => false,
            },
            _ => false,
        }
    }

    /// Returns the SqlType for this DataType, or None if [`DataType::None`] (which is valid for any
    /// type)
    pub fn sql_type(&self) -> Option<SqlType> {
        use SqlType::*;
        match self {
            Self::None => None,
            Self::Int(_) => Some(Int(None)),
            Self::UnsignedInt(_) => Some(UnsignedInt(None)),
            Self::BigInt(_) => Some(Bigint(None)),
            Self::UnsignedBigInt(_) => Some(UnsignedBigint(None)),
            Self::Float(_, _) => Some(Float),
            Self::Double(_, _) => Some(Real),
            Self::Text(_) => Some(Text),
            Self::TinyText(_) => Some(Tinytext),
            Self::Timestamp(_) => Some(Timestamp),
            Self::Time(_) => Some(Time),
        }
    }

    /// Attempt to coerce the given DataType to a value of the given `SqlType`.
    ///
    /// Currently, this entails:
    ///
    /// * Coercing values to the type they already are
    /// * Parsing strings ([`Text`], [`Tinytext`], [`Mediumtext`]) as integers
    /// * Parsing strings ([`Text`], [`Tinytext`], [`Mediumtext`]) as timestamps
    /// * Changing numeric type sizes (bigint -> int, int -> bigint, etc)
    /// * Converting [`Real`]s with a zero fractional part to an integer
    /// * Removing information from date/time data types (timestamps to dates or times)
    ///
    /// More coercions will likely need to be added in the future
    ///
    /// # Examples
    ///
    /// Reals can be converted to integers
    ///
    /// ```rust
    /// use noria::DataType;
    /// use nom_sql::SqlType;
    ///
    /// let real = DataType::Double(123.0, 0);
    /// let int = real.coerce_to(&SqlType::Int(None)).unwrap();
    /// assert_eq!(int.into_owned(), DataType::Int(123));
    /// ```
    ///
    /// Text can be parsed as a timestamp using the SQL `%Y-%m-%d %H:%M:%S` format:
    ///
    /// ```rust
    /// use noria::DataType;
    /// use nom_sql::SqlType;
    /// use chrono::NaiveDate;
    /// use std::borrow::Borrow;
    /// use std::convert::TryFrom;
    ///
    /// let text = DataType::try_from("2021-01-26 10:20:37").unwrap();
    /// let timestamp = text.coerce_to(&SqlType::Timestamp).unwrap();
    /// assert_eq!(
    ///   timestamp.into_owned(),
    ///   DataType::Timestamp(NaiveDate::from_ymd(2021, 01, 26).and_hms(10, 20, 37))
    /// );
    /// ```
    pub fn coerce_to<'a>(&'a self, ty: &SqlType) -> ReadySetResult<Cow<'a, Self>> {
        let mk_err = |message: String, source: Option<anyhow::Error>| {
            ReadySetError::DataTypeConversionError {
                val: format!("{:?}", self),
                src_type: "DataType".to_string(),
                target_type: format!("{:?}", ty),
                details: format!(
                    "{}{}",
                    message,
                    source
                        .map(|err| format!(" (caused by: {})", err))
                        .unwrap_or_default()
                ),
            }
        };

        macro_rules! convert_numeric {
            ($dt: expr, $source_ty: ty, $target_ty: ty) => {{
                Ok(Cow::Owned(
                    <$target_ty>::try_from(<$source_ty>::try_from($dt)?)
                        .map_err(|e| {
                            mk_err("Could not convert numeric types".to_owned(), Some(e.into()))
                        })?
                        .into(),
                ))
            }};
        }

        macro_rules! convert_boolean {
            ($val: expr) => {{
                Ok(Cow::Owned(DataType::from($val != 0)))
            }};
        }

        use SqlType::*;
        match (self, self.sql_type(), ty) {
            (_, None, _) => Ok(Cow::Borrowed(self)),
            (_, Some(src_type), tgt_type) if src_type == *tgt_type => Ok(Cow::Borrowed(self)),
            (_, Some(Text | Tinytext | Mediumtext), Text | Tinytext | Mediumtext) => {
                Ok(Cow::Borrowed(self))
            }
            // Per https://dev.mysql.com/doc/refman/8.0/en/numeric-type-syntax.html, the "number"
            // argument to integer types only controls the display width, not the max length
            (_, Some(Int(_)), Int(_))
            | (_, Some(Bigint(_)), Bigint(_))
            | (_, Some(UnsignedInt(_)), UnsignedInt(_))
            | (_, Some(UnsignedBigint(_)), UnsignedBigint(_)) => Ok(Cow::Borrowed(self)),
            (_, Some(Int(_)), Tinyint(_)) => convert_numeric!(self, i32, i8),
            (_, Some(Int(_)), UnsignedTinyint(_)) => convert_numeric!(self, i32, u8),
            (_, Some(Int(_)), Smallint(_)) => convert_numeric!(self, i32, i16),
            (_, Some(Int(_)), UnsignedSmallint(_)) => convert_numeric!(self, i32, u16),
            (_, Some(Bigint(_)), Tinyint(_)) => convert_numeric!(self, i64, i8),
            (_, Some(Bigint(_)), UnsignedTinyint(_)) => convert_numeric!(self, i64, u8),
            (_, Some(Bigint(_)), Smallint(_)) => convert_numeric!(self, i64, i16),
            (_, Some(Bigint(_)), UnsignedSmallint(_)) => convert_numeric!(self, i64, u16),
            (_, Some(Bigint(_)), Int(_)) => convert_numeric!(self, i64, i32),
            (_, Some(Bigint(_)), UnsignedInt(_)) => convert_numeric!(self, i64, u32),
            (_, Some(Bigint(_)), UnsignedBigint(_)) => convert_numeric!(self, i64, u64),
            (Self::Int(n), _, Bool) => convert_boolean!(*n),
            (Self::UnsignedInt(n), _, Bool) => convert_boolean!(*n),
            (Self::BigInt(n), _, Bool) => convert_boolean!(*n),
            (Self::UnsignedBigInt(n), _, Bool) => convert_boolean!(*n),
            (_, Some(Float), Float | Real) => Ok(Cow::Borrowed(self)),
            (_, Some(Real), Double) => Ok(Cow::Borrowed(self)),
            (_, Some(Text | Tinytext | Mediumtext), Varchar(max_len)) => {
                let actual_len = <&str>::try_from(self)?.len();
                if actual_len <= (*max_len).into() {
                    Ok(Cow::Borrowed(self))
                } else {
                    Err(mk_err(
                        format!(
                            "Value ({} characters long) longer than maximum length of {} characters",
                            actual_len,
                            max_len
                        ),
                        None
                    ))
                }
            }
            (_, Some(Text | Tinytext | Mediumtext | Varchar(_)), Char(Some(len))) => {
                let actual_len = <&str>::try_from(self)?.len();
                if actual_len <= usize::from(*len) {
                    Ok(Cow::Borrowed(self))
                } else {
                    Err(mk_err(
                        format!(
                            "Value ({} characters long) is not required length of {} characters",
                            actual_len, len
                        ),
                        None,
                    ))
                }
            }
            (_, Some(Text | Tinytext | Mediumtext | Varchar(_)), Char(None)) => {
                Ok(Cow::Borrowed(self))
            }
            (_, Some(Text | Tinytext | Mediumtext | Varchar(_)), Timestamp | DateTime(_)) => {
                NaiveDateTime::parse_from_str(<&str>::try_from(self)?, TIMESTAMP_FORMAT)
                    .map_err(|e| {
                        mk_err(
                            "Could not parse value as timestamp".to_owned(),
                            Some(e.into()),
                        )
                    })
                    .map(Self::Timestamp)
                    .map(Cow::Owned)
            }
            (_, Some(Text | Tinytext | Mediumtext | Varchar(_)), Date) => {
                let text: &str = <&str>::try_from(self)?;
                NaiveDate::parse_from_str(text, DATE_FORMAT)
                    .or_else(|_e| {
                        NaiveDateTime::parse_from_str(text, TIMESTAMP_FORMAT).map(|dt| dt.date())
                    })
                    .map_err(|e| mk_err("Could not parse value as date".to_owned(), Some(e.into())))
                    .map(|date| {
                        Self::Timestamp(NaiveDateTime::new(date, NaiveTime::from_hms(0, 0, 0)))
                    })
                    .map(Cow::Owned)
            }
            (_, Some(Text | Tinytext | Mediumtext | Varchar(_)), Time) => {
                match <&str>::try_from(self)?.parse() {
                    Ok(t) => Ok(Cow::Owned(Self::Time(Arc::new(t)))),
                    Err(mysql_time::ConvertError::ParseError) => {
                        Ok(Cow::Owned(Self::Time(Arc::new(Default::default()))))
                    }
                    Err(e) => Err(mk_err(
                        "Could not parse value as time".to_owned(),
                        Some(e.into()),
                    )),
                }
            }
            (_, Some(Int(_) | Bigint(_) | Real | Float), Time) => {
                MysqlTime::try_from(<f64>::try_from(self)?)
                    .map_err(|e| mk_err("Could not parse value as time".to_owned(), Some(e.into())))
                    .map(Arc::new)
                    .map(Self::Time)
                    .map(Cow::Owned)
            }
            (Self::Timestamp(ts), Some(Timestamp), Text | Tinytext | Mediumtext | Varchar(_)) => {
                Ok(Cow::Owned(ts.format(TIMESTAMP_FORMAT).to_string().into()))
            }
            (Self::Time(ts), Some(Time), Text | Tinytext | Mediumtext | Varchar(_)) => {
                Ok(Cow::Owned(ts.to_string().into()))
            }
            (Self::Timestamp(ts), Some(Timestamp), Date) => {
                Ok(Cow::Owned(Self::Timestamp(ts.date().and_hms(0, 0, 0))))
            }
            (Self::Timestamp(ts), Some(Timestamp), Time) => {
                Ok(Cow::Owned(Self::Time(Arc::new(ts.time().into()))))
            }
            (_, Some(Int(_)), Bigint(_)) => Ok(Cow::Owned(DataType::BigInt(i64::try_from(self)?))),
            (Self::Float(f, _), Some(Float), Tinyint(_) | Smallint(_) | Int(_)) => {
                Ok(Cow::Owned(DataType::Int(f.round() as i32)))
            }
            (Self::Float(f, _), Some(_), Bigint(_)) => {
                Ok(Cow::Owned(DataType::BigInt(f.round() as i64)))
            }
            (Self::Float(f, prec), Some(_), Double) => {
                Ok(Cow::Owned(DataType::Double(*f as f64, *prec)))
            }
            (
                Self::Float(f, _),
                Some(Float),
                UnsignedTinyint(_) | UnsignedSmallint(_) | UnsignedInt(_),
            ) => Ok(Cow::Owned(DataType::UnsignedInt(
                u32::try_from(f.round() as i32).map_err(|e| {
                    mk_err("Could not convert numeric types".to_owned(), Some(e.into()))
                })?,
            ))),
            (Self::Double(f, _), Some(Real), Tinyint(_) | Smallint(_) | Int(_)) => Ok(Cow::Owned(
                DataType::Int(i32::try_from(f.round() as i64).map_err(|e| {
                    mk_err("Could not convert numeric types".to_owned(), Some(e.into()))
                })?),
            )),
            (Self::Double(f, prec), Some(_), Float) => {
                let float = *f as f32;
                if float.is_finite() {
                    Ok(Cow::Owned(DataType::Float(*f as f32, *prec)))
                } else {
                    Err(mk_err(
                        "Could not convert numeric types: infinite value is not supported"
                            .to_owned(),
                        None,
                    ))
                }
            }
            (Self::Double(f, _), Some(_), Bigint(_)) => {
                Ok(Cow::Owned(DataType::BigInt(f.round() as i64)))
            }
            (
                Self::Double(f, _),
                Some(Real),
                UnsignedTinyint(_) | UnsignedSmallint(_) | UnsignedInt(_),
            ) => Ok(Cow::Owned(DataType::UnsignedInt(
                u32::try_from(f.round() as i64).map_err(|e| {
                    mk_err("Could not convert numeric types".to_owned(), Some(e.into()))
                })?,
            ))),
            (Self::Double(f, _), Some(Real), UnsignedBigint(_)) => Ok(Cow::Owned(
                DataType::UnsignedBigInt(u64::try_from(f.round() as i64).map_err(|e| {
                    mk_err("Could not convert numeric types".to_owned(), Some(e.into()))
                })?),
            )),
            (_, Some(Text | Tinytext | Mediumtext | Varchar(_)), Tinyint(_)) => {
                <&str>::try_from(self)?
                    .parse::<i8>()
                    .map(|x| (Cow::Owned(DataType::from(x))))
                    .map_err(|e| {
                        mk_err("Could not parse value as number".to_owned(), Some(e.into()))
                    })
            }
            (_, Some(Text | Tinytext | Mediumtext | Varchar(_)), Smallint(_)) => {
                <&str>::try_from(self)?
                    .parse::<i16>()
                    .map(|x| (Cow::Owned(DataType::from(x))))
                    .map_err(|e| {
                        mk_err("Could not parse value as number".to_owned(), Some(e.into()))
                    })
            }
            (_, Some(Text | Tinytext | Mediumtext | Varchar(_)), Int(_)) => <&str>::try_from(self)?
                .parse::<i32>()
                .map(|x| (Cow::Owned(DataType::from(x))))
                .map_err(|e| mk_err("Could not parse value as number".to_owned(), Some(e.into()))),
            (_, Some(Text | Tinytext | Mediumtext | Varchar(_)), Bigint(_)) => {
                <&str>::try_from(self)?
                    .parse::<i64>()
                    .map(|x| (Cow::Owned(DataType::from(x))))
                    .map_err(|e| {
                        mk_err("Could not parse value as number".to_owned(), Some(e.into()))
                    })
            }
            (_, Some(Text | Tinytext | Mediumtext | Varchar(_)), UnsignedTinyint(_)) => {
                <&str>::try_from(self)?
                    .parse::<u8>()
                    .map(|x| (Cow::Owned(DataType::from(x))))
                    .map_err(|e| {
                        mk_err("Could not parse value as number".to_owned(), Some(e.into()))
                    })
            }
            (_, Some(Text | Tinytext | Mediumtext | Varchar(_)), UnsignedSmallint(_)) => {
                <&str>::try_from(self)?
                    .parse::<u16>()
                    .map(|x| (Cow::Owned(DataType::from(x))))
                    .map_err(|e| {
                        mk_err("Could not parse value as number".to_owned(), Some(e.into()))
                    })
            }
            (_, Some(Text | Tinytext | Mediumtext | Varchar(_)), UnsignedInt(_)) => {
                <&str>::try_from(self)?
                    .parse::<u32>()
                    .map(|x| (Cow::Owned(DataType::from(x))))
                    .map_err(|e| {
                        mk_err("Could not parse value as number".to_owned(), Some(e.into()))
                    })
            }
            (_, Some(Text | Tinytext | Mediumtext | Varchar(_)), UnsignedBigint(_)) => {
                <&str>::try_from(self)?
                    .parse::<u64>()
                    .map(|x| (Cow::Owned(DataType::from(x))))
                    .map_err(|e| {
                        mk_err("Could not parse value as number".to_owned(), Some(e.into()))
                    })
            }
            (_, Some(Text | Tinytext | Mediumtext | Varchar(_)), Json) => {
                // TODO(grfn): Validate JSON here
                Ok(Cow::Borrowed(self))
            }
            (_, Some(_), _) => Err(mk_err("Cannot coerce with these types".to_owned(), None)),
        }
    }

    /// Returns Some(&self) if self is not [`DataType::None`]
    ///
    /// # Examples
    ///
    /// ```
    /// use noria::DataType;
    ///
    /// assert_eq!(DataType::Int(12).non_null(), Some(&DataType::Int(12)));
    /// assert_eq!(DataType::None.non_null(), None);
    /// ```
    pub fn non_null(&self) -> Option<&Self> {
        if self.is_none() {
            None
        } else {
            Some(self)
        }
    }

    /// Returns Some(self) if self is not [`DataType::None`]
    ///
    /// # Examples
    ///
    /// ```
    /// use noria::DataType;
    ///
    /// assert_eq!(DataType::Int(12).into_non_null(), Some(DataType::Int(12)));
    /// assert_eq!(DataType::None.into_non_null(), None);
    /// ```
    pub fn into_non_null(self) -> Option<Self> {
        if self.is_none() {
            None
        } else {
            Some(self)
        }
    }
}

impl PartialEq for DataType {
    fn eq(&self, other: &DataType) -> bool {
        unsafe {
            use std::slice;
            // if the two datatypes are byte-for-byte identical, they're the same.
            let a: &[u8] =
                slice::from_raw_parts(&*self as *const _ as *const u8, mem::size_of::<Self>());
            let b: &[u8] =
                slice::from_raw_parts(&*other as *const _ as *const u8, mem::size_of::<Self>());
            if a == b {
                return true;
            }
        }

        match (self, other) {
            (&DataType::Text(ref a), &DataType::Text(ref b)) => a == b,
            (&DataType::TinyText(ref a), &DataType::TinyText(ref b)) => a == b,
            (&DataType::Text(..), &DataType::TinyText(..))
            | (&DataType::TinyText(..), &DataType::Text(..)) => {
                // this unwrap should be safe because no error path in try_from for &str on Text or TinyText
                #[allow(clippy::unwrap_used)]
                let a: &str = <&str>::try_from(self).unwrap();
                // this unwrap should be safe because no error path in try_from for &str on Text or TinyText
                #[allow(clippy::unwrap_used)]
                let b: &str = <&str>::try_from(other).unwrap();
                a == b
            }
            (&DataType::Text(..) | &DataType::TinyText(..), &DataType::Timestamp(dt)) => {
                // this unwrap should be safe because no error path in try_from for &str on Text or TinyText
                #[allow(clippy::unwrap_used)]
                let a = <&str>::try_from(self).unwrap();
                NaiveDateTime::parse_from_str(a, TIMESTAMP_FORMAT)
                    .map(|other_dt| dt.eq(&other_dt))
                    .unwrap_or(false)
            }
            (&DataType::Text(..) | &DataType::TinyText(..), &DataType::Time(ref t)) => {
                // this unwrap should be safe because no error path in try_from for &str on Text or TinyText
                #[allow(clippy::unwrap_used)]
                let a = <&str>::try_from(self).unwrap();
                a.parse()
                    .map(|other_t: MysqlTime| t.as_ref().eq(&other_t))
                    .unwrap_or(false)
            }
            (&DataType::BigInt(a), &DataType::BigInt(b)) => a == b,
            (&DataType::UnsignedBigInt(a), &DataType::UnsignedBigInt(b)) => a == b,
            (&DataType::Int(a), &DataType::Int(b)) => a == b,
            (&DataType::UnsignedInt(a), &DataType::UnsignedInt(b)) => a == b,
            (&DataType::UnsignedBigInt(..), &DataType::Int(..))
            | (&DataType::UnsignedBigInt(..), &DataType::UnsignedInt(..))
            | (&DataType::UnsignedBigInt(..), &DataType::BigInt(..))
            | (&DataType::UnsignedInt(..), &DataType::Int(..))
            | (&DataType::UnsignedInt(..), &DataType::BigInt(..))
            | (&DataType::UnsignedInt(..), &DataType::UnsignedBigInt(..))
            | (&DataType::BigInt(..), &DataType::Int(..))
            | (&DataType::BigInt(..), &DataType::UnsignedInt(..))
            | (&DataType::BigInt(..), &DataType::UnsignedBigInt(..))
            | (&DataType::Int(..), &DataType::UnsignedInt(..))
            | (&DataType::Int(..), &DataType::UnsignedBigInt(..))
            | (&DataType::Int(..), &DataType::BigInt(..)) => {
                // this unwrap should be safe because no error path in try_from for i128 (&i128) on Int, BigInt, UnsignedInt, and UnsignedBigInt
                #[allow(clippy::unwrap_used)]
                let a: i128 = <i128>::try_from(self).unwrap();
                // this unwrap should be safe because no error path in try_from for i128 (&i128) on Int, BigInt, UnsignedInt, and UnsignedBigInt
                #[allow(clippy::unwrap_used)]
                let b: i128 = <i128>::try_from(other).unwrap();
                a == b
            }
            (&DataType::Float(fa, pa), &DataType::Float(fb, pb)) => {
                // We need to compare the *bit patterns* of the floats so that our Hash matches our
                // Eq
                fa.to_bits() == fb.to_bits() && pa == pb
            }
            (&DataType::Float(fa, pa), &DataType::Double(fb, pb)) => {
                // We need to compare the *bit patterns* of the floats so that our Hash matches our
                // Eq
                fa.to_bits() == (fb as f32).to_bits() && pa == pb
            }
            (&DataType::Double(fa, pa), &DataType::Double(fb, pb)) => {
                // We need to compare the *bit patterns* of the floats so that our Hash matches our
                // Eq
                fa.to_bits() == fb.to_bits() && pa == pb
            }
            (&DataType::Double(fa, pa), &DataType::Float(fb, pb)) => {
                // We need to compare the *bit patterns* of the floats so that our Hash matches our
                // Eq
                fa.to_bits() == (fb as f64).to_bits() && pa == pb
            }
            (
                &DataType::Timestamp(_) | &DataType::Time(_),
                &DataType::Text(..) | &DataType::TinyText(..),
            ) => other == self,
            (&DataType::Timestamp(tsa), &DataType::Timestamp(tsb)) => tsa == tsb,
            (&DataType::Time(ref ta), &DataType::Time(ref tb)) => ta.as_ref() == tb.as_ref(),
            (&DataType::None, &DataType::None) => true,

            _ => false,
        }
    }
}

use crate::errors::internal_err;
use mysql_time::MysqlTime;
use std::cmp::Ordering;
use std::sync::Arc;

impl PartialOrd for DataType {
    fn partial_cmp(&self, other: &DataType) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for DataType {
    fn cmp(&self, other: &DataType) -> Ordering {
        match (self, other) {
            (&DataType::Text(ref a), &DataType::Text(ref b)) => a.cmp(b),
            (&DataType::TinyText(ref a), &DataType::TinyText(ref b)) => a.cmp(b),
            (&DataType::Text(..), &DataType::TinyText(..))
            | (&DataType::TinyText(..), &DataType::Text(..)) => {
                // this unwrap should be safe because no error path in try_from for &str on Text or TinyText
                #[allow(clippy::unwrap_used)]
                let a: &str = <&str>::try_from(self).unwrap();
                // this unwrap should be safe because no error path in try_from for &str on Text or TinyText
                #[allow(clippy::unwrap_used)]
                let b: &str = <&str>::try_from(other).unwrap();
                a.cmp(b)
            }
            (&DataType::Text(..) | &DataType::TinyText(..), &DataType::Timestamp(ref other_dt)) => {
                // this unwrap should be safe because no error path in try_from for &str on Text or TinyText
                #[allow(clippy::unwrap_used)]
                let a: &str = <&str>::try_from(self).unwrap();
                NaiveDateTime::parse_from_str(a, TIMESTAMP_FORMAT)
                    .map(|dt| dt.cmp(other_dt))
                    .unwrap_or(Ordering::Greater)
            }
            (&DataType::Text(..) | &DataType::TinyText(..), &DataType::Time(ref other_t)) => {
                // this unwrap should be safe because no error path in try_from for &str on Text or TinyText
                #[allow(clippy::unwrap_used)]
                let a = <&str>::try_from(self).unwrap().parse();
                a.map(|t: MysqlTime| t.cmp(other_t.as_ref()))
                    .unwrap_or(Ordering::Greater)
            }
            (&DataType::Text(..) | &DataType::TinyText(..), _) => Ordering::Greater,
            (
                &DataType::Time(_) | &DataType::Timestamp(_),
                &DataType::Text(..) | &DataType::TinyText(..),
            ) => other.cmp(self).reverse(),
            (_, &DataType::Text(..) | &DataType::TinyText(..)) => Ordering::Less,
            (&DataType::BigInt(a), &DataType::BigInt(ref b)) => a.cmp(b),
            (&DataType::UnsignedBigInt(a), &DataType::UnsignedBigInt(ref b)) => a.cmp(b),
            (&DataType::Int(a), &DataType::Int(b)) => a.cmp(&b),
            (&DataType::UnsignedInt(a), &DataType::UnsignedInt(b)) => a.cmp(&b),
            (&DataType::BigInt(..), &DataType::Int(..))
            | (&DataType::Int(..), &DataType::BigInt(..))
            | (&DataType::BigInt(..), &DataType::UnsignedInt(..))
            | (&DataType::UnsignedInt(..), &DataType::BigInt(..))
            | (&DataType::BigInt(..), &DataType::UnsignedBigInt(..))
            | (&DataType::UnsignedBigInt(..), &DataType::BigInt(..))
            | (&DataType::UnsignedBigInt(..), &DataType::UnsignedInt(..))
            | (&DataType::UnsignedInt(..), &DataType::UnsignedBigInt(..))
            | (&DataType::Int(..), &DataType::UnsignedBigInt(..))
            | (&DataType::UnsignedBigInt(..), &DataType::Int(..))
            | (&DataType::UnsignedInt(..), &DataType::Int(..))
            | (&DataType::Int(..), &DataType::UnsignedInt(..)) => {
                // this unwrap should be safe because no error path in try_from for i128 (&i128) on Int, BigInt, UnsignedInt, and UnsignedBigInt
                #[allow(clippy::unwrap_used)]
                let a: i128 = <i128>::try_from(self).unwrap();
                // this unwrap should be safe because no error path in try_from for i128 (&i128) on Int, BigInt, UnsignedInt, and UnsignedBigInt
                #[allow(clippy::unwrap_used)]
                let b: i128 = <i128>::try_from(other).unwrap();
                a.cmp(&b)
            }
            (&DataType::Float(fa, _), &DataType::Float(fb, _)) => fa.total_cmp(&fb),
            (&DataType::Float(fa, _), &DataType::Double(fb, _)) => fa.total_cmp(&(fb as f32)),
            (&DataType::Double(fa, _), &DataType::Float(fb, _)) => fa.total_cmp(&(fb as f64)),
            (&DataType::Double(fa, _), &DataType::Double(fb, _)) => fa.total_cmp(&fb),
            (&DataType::Timestamp(tsa), &DataType::Timestamp(ref tsb)) => tsa.cmp(tsb),
            (&DataType::Time(ref ta), &DataType::Time(ref tb)) => ta.cmp(tb),
            (&DataType::None, &DataType::None) => Ordering::Equal,

            // Convert ints to f32 and cmp against Float.
            (&DataType::Int(..), &DataType::Float(b, ..))
            | (&DataType::UnsignedInt(..), &DataType::Float(b, ..))
            | (&DataType::BigInt(..), &DataType::Float(b, ..))
            | (&DataType::UnsignedBigInt(..), &DataType::Float(b, ..)) => {
                // this unwrap should be safe because no error path in try_from for i128 (&i128) on Int, BigInt, UnsignedInt, and UnsignedBigInt
                #[allow(clippy::unwrap_used)]
                let a: i128 = <i128>::try_from(self).unwrap();

                (a as f32).total_cmp(&b)
            }
            // Convert ints to double and cmp against Real.
            (&DataType::Int(..), &DataType::Double(b, ..))
            | (&DataType::UnsignedInt(..), &DataType::Double(b, ..))
            | (&DataType::BigInt(..), &DataType::Double(b, ..))
            | (&DataType::UnsignedBigInt(..), &DataType::Double(b, ..)) => {
                // this unwrap should be safe because no error path in try_from for i128 (&i128) on Int, BigInt, UnsignedInt, and UnsignedBigInt
                #[allow(clippy::unwrap_used)]
                let a: i128 = <i128>::try_from(self).unwrap();

                (a as f64).total_cmp(&b)
            }
            // order Ints, Reals, Text, Timestamps, None
            (&DataType::Int(..), _)
            | (&DataType::UnsignedInt(..), _)
            | (&DataType::BigInt(..), _)
            | (&DataType::UnsignedBigInt(..), _) => Ordering::Greater,
            (&DataType::Float(a, ..), &DataType::Int(..))
            | (&DataType::Float(a, ..), &DataType::UnsignedInt(..))
            | (&DataType::Float(a, ..), &DataType::BigInt(..))
            | (&DataType::Float(a, ..), &DataType::UnsignedBigInt(..)) => {
                // this unwrap should be safe because no error path in try_from for i128 (&i128) on Int, BigInt, UnsignedInt, and UnsignedBigInt
                #[allow(clippy::unwrap_used)]
                let b: i128 = <i128>::try_from(other).unwrap();

                a.total_cmp(&(b as f32))
            }
            (&DataType::Double(a, ..), &DataType::Int(..))
            | (&DataType::Double(a, ..), &DataType::UnsignedInt(..))
            | (&DataType::Double(a, ..), &DataType::BigInt(..))
            | (&DataType::Double(a, ..), &DataType::UnsignedBigInt(..)) => {
                // this unwrap should be safe because no error path in try_from for i128 (&i128) on Int, BigInt, UnsignedInt, and UnsignedBigInt
                #[allow(clippy::unwrap_used)]
                let b: i128 = <i128>::try_from(other).unwrap();

                a.total_cmp(&(b as f64))
            }
            (&DataType::Double(..) | &DataType::Float(..), _) => Ordering::Greater,
            (&DataType::Timestamp(..) | DataType::Time(_), _) => Ordering::Greater,
            (&DataType::None, _) => Ordering::Greater,
        }
    }
}

impl Hash for DataType {
    fn hash<H: Hasher>(&self, state: &mut H) {
        // The default derived hash function also hashes the variant tag, which turns out to be
        // rather expensive. This version could (but probably won't) have a higher rate of
        // collisions, but the decreased overhead is worth it.
        match *self {
            DataType::None => {}
            DataType::Int(..) | DataType::BigInt(..) => {
                // this unwrap should be safe because no error path in try_from for i64 (&i64) on Int and BigInt
                #[allow(clippy::unwrap_used)]
                let n: i64 = <i64>::try_from(self).unwrap();
                n.hash(state)
            }
            DataType::UnsignedInt(..) | DataType::UnsignedBigInt(..) => {
                // this unwrap should be safe because no error path in try_from for u64 (&u64) on UnsignedInt and UnsignedBigInt
                #[allow(clippy::unwrap_used)]
                let n: u64 = <u64>::try_from(self).unwrap();
                n.hash(state)
            }
            DataType::Float(f, p) => {
                f.to_bits().hash(state);
                p.hash(state);
            }
            DataType::Double(f, p) => {
                f.to_bits().hash(state);
                p.hash(state);
            }
            DataType::Text(..) | DataType::TinyText(..) => {
                // this unwrap should be safe because no error path in try_from for &str on Text or TinyText
                #[allow(clippy::unwrap_used)]
                let t: &str = <&str>::try_from(self).unwrap();
                t.hash(state)
            }
            DataType::Timestamp(ts) => ts.hash(state),
            DataType::Time(ref t) => t.hash(state),
        }
    }
}

impl<T> From<Option<T>> for DataType
where
    DataType: From<T>,
{
    fn from(opt: Option<T>) -> Self {
        match opt {
            Some(t) => DataType::from(t),
            None => DataType::None,
        }
    }
}

impl TryFrom<i128> for DataType {
    type Error = ReadySetError;

    fn try_from(s: i128) -> Result<Self, Self::Error> {
        if s >= std::i32::MIN.into() && s <= std::i32::MAX.into() {
            Ok(DataType::Int(s as i32))
        } else if s >= std::u32::MIN.into() && s <= std::u32::MAX.into() {
            Ok(DataType::UnsignedInt(s as u32))
        } else if s >= std::i64::MIN.into() && s <= std::i64::MAX.into() {
            Ok(DataType::BigInt(s as i64))
        } else if s >= std::u64::MIN.into() && s <= std::u64::MAX.into() {
            Ok(DataType::UnsignedBigInt(s as u64))
        } else {
            Err(Self::Error::DataTypeConversionError {
                val: s.to_string(),
                src_type: "i128".to_string(),
                target_type: "DataType".to_string(),
                details: "".to_string(),
            })
        }
    }
}

impl From<i64> for DataType {
    fn from(s: i64) -> Self {
        DataType::BigInt(s)
    }
}

impl From<u64> for DataType {
    fn from(s: u64) -> Self {
        DataType::UnsignedBigInt(s)
    }
}

impl From<i8> for DataType {
    fn from(s: i8) -> Self {
        DataType::Int(s.into())
    }
}

impl From<u8> for DataType {
    fn from(s: u8) -> Self {
        DataType::UnsignedInt(s.into())
    }
}

impl From<i16> for DataType {
    fn from(s: i16) -> Self {
        DataType::Int(s.into())
    }
}

impl From<u16> for DataType {
    fn from(s: u16) -> Self {
        DataType::UnsignedInt(s.into())
    }
}

impl From<i32> for DataType {
    fn from(s: i32) -> Self {
        DataType::Int(s)
    }
}

impl From<u32> for DataType {
    fn from(s: u32) -> Self {
        DataType::UnsignedInt(s)
    }
}

impl From<usize> for DataType {
    fn from(s: usize) -> Self {
        DataType::UnsignedBigInt(s as u64)
    }
}

impl TryFrom<f32> for DataType {
    type Error = ReadySetError;

    fn try_from(f: f32) -> Result<Self, Self::Error> {
        if !f.is_finite() {
            return Err(Self::Error::DataTypeConversionError {
                val: f.to_string(),
                src_type: "f32".to_string(),
                target_type: "DataType".to_string(),
                details: "".to_string(),
            });
        }

        Ok(DataType::Float(f, u8::MAX))
    }
}

impl TryFrom<f64> for DataType {
    type Error = ReadySetError;

    fn try_from(f: f64) -> Result<Self, Self::Error> {
        if !f.is_finite() {
            return Err(Self::Error::DataTypeConversionError {
                val: f.to_string(),
                src_type: "f64".to_string(),
                target_type: "DataType".to_string(),
                details: "".to_string(),
            });
        }

        Ok(DataType::Double(f, u8::MAX))
    }
}

/// Booleans are represented as `u32`s which are equal to either 0 or 1
impl From<bool> for DataType {
    fn from(b: bool) -> Self {
        DataType::from(b as u32)
    }
}

impl<'a> From<&'a DataType> for DataType {
    fn from(dt: &'a DataType) -> Self {
        dt.clone()
    }
}

impl<'a> TryFrom<&'a Literal> for DataType {
    type Error = ReadySetError;

    fn try_from(l: &'a Literal) -> Result<Self, Self::Error> {
        match l {
            Literal::Null => Ok(DataType::None),
            Literal::Integer(i) => Ok((*i as i64).into()),
            Literal::String(s) => Ok(s.as_str().into()),
            Literal::CurrentTimestamp | Literal::CurrentTime => {
                let ts = chrono::Local::now().naive_local();
                Ok(DataType::Timestamp(ts))
            }
            Literal::CurrentDate => Ok(DataType::from(chrono::Local::now().naive_local().date())),
            Literal::Float(ref float) => Ok(DataType::Float(float.value, float.precision)),
            Literal::Double(ref double) => Ok(DataType::Double(double.value, double.precision)),
            Literal::Blob(b) => DataType::try_from(b.as_slice()),
            Literal::Placeholder(_) => {
                internal!("Tried to convert a Placeholder literal to a DataType")
            }
        }
    }
}

impl TryFrom<Literal> for DataType {
    type Error = ReadySetError;

    fn try_from(l: Literal) -> Result<Self, Self::Error> {
        (&l).try_into()
    }
}

impl TryFrom<DataType> for Literal {
    type Error = ReadySetError;

    fn try_from(dt: DataType) -> Result<Self, Self::Error> {
        match dt {
            DataType::None => Ok(Literal::Null),
            DataType::Int(i) => Ok(Literal::Integer(i as _)),
            DataType::UnsignedInt(i) => Ok(Literal::Integer(i as _)),
            DataType::BigInt(i) => Ok(Literal::Integer(i)),
            DataType::UnsignedBigInt(i) => Ok(Literal::Integer(i as _)),
            DataType::Float(value, precision) => Ok(Literal::Float(Float { value, precision })),
            DataType::Double(value, precision) => Ok(Literal::Double(Double { value, precision })),
            DataType::Text(_) => Ok(Literal::String(String::try_from(dt)?)),
            DataType::TinyText(_) => Ok(Literal::String(String::try_from(dt)?)),
            DataType::Timestamp(_) => Ok(Literal::String(String::try_from(
                dt.coerce_to(&SqlType::Text)?.as_ref(),
            )?)),
            DataType::Time(_) => Ok(Literal::String(String::try_from(
                dt.coerce_to(&SqlType::Text)?.as_ref(),
            )?)),
        }
    }
}

impl From<NaiveTime> for DataType {
    fn from(t: NaiveTime) -> Self {
        DataType::Time(Arc::new(t.into()))
    }
}

impl From<MysqlTime> for DataType {
    fn from(t: MysqlTime) -> Self {
        DataType::Time(Arc::new(t))
    }
}

impl From<NaiveDate> for DataType {
    fn from(dt: NaiveDate) -> Self {
        DataType::Timestamp(dt.and_hms(0, 0, 0))
    }
}

impl From<NaiveDateTime> for DataType {
    fn from(dt: NaiveDateTime) -> Self {
        DataType::Timestamp(dt)
    }
}

impl<'a> TryFrom<&'a DataType> for NaiveDateTime {
    type Error = ReadySetError;

    fn try_from(data: &'a DataType) -> Result<Self, Self::Error> {
        match *data {
            DataType::Timestamp(ref dt) => Ok(*dt),
            _ => Err(Self::Error::DataTypeConversionError {
                val: format!("{:?}", data),
                src_type: "DataType".to_string(),
                target_type: "NaiveDateTime".to_string(),
                details: "".to_string(),
            }),
        }
    }
}

impl<'a> TryFrom<&'a DataType> for NaiveDate {
    type Error = ReadySetError;

    fn try_from(data: &'a DataType) -> Result<Self, Self::Error> {
        match *data {
            DataType::Timestamp(ref dt) => Ok(dt.date()),
            _ => Err(Self::Error::DataTypeConversionError {
                val: format!("{:?}", data),
                src_type: "DataType".to_string(),
                target_type: "NaiveDate".to_string(),
                details: "".to_string(),
            }),
        }
    }
}

impl<'a> TryFrom<&'a DataType> for MysqlTime {
    type Error = ReadySetError;

    fn try_from(data: &'a DataType) -> Result<Self, Self::Error> {
        match *data {
            DataType::Time(ref mysql_time) => Ok(*mysql_time.as_ref()),
            _ => Err(Self::Error::DataTypeConversionError {
                val: format!("{:?}", data),
                src_type: "DataType".to_string(),
                target_type: "MysqlTime".to_string(),
                details: "".to_string(),
            }),
        }
    }
}

// This conversion has many unwraps, but all of them are expected to be safe,
// because DataType variants (i.e. `Text` and `TinyText`) constructors are all
// generated from valid UTF-8 strings, or the constructor fails (e.g. TryFrom &[u8]).
// Thus, we can safely generate a &str from a DataType.
impl<'a> TryFrom<&'a DataType> for &'a str {
    type Error = ReadySetError;

    fn try_from(data: &'a DataType) -> Result<Self, Self::Error> {
        match *data {
            DataType::Text(ref s) => Ok(s.to_str().map_err(|e| {
                ReadySetError::BadRequest(format!("unable to create str from &ArcCStr: {}", e))
            })?),
            DataType::TinyText(ref bts) => {
                if bts[TINYTEXT_WIDTH - 1] == 0 {
                    // NULL terminated CStr
                    use std::ffi::CStr;
                    // this is safe, since we are checking that bts[TINYTEXT_WIDTH - 1] == 0
                    // (so there is at least one 0 element in bts)
                    #[allow(clippy::unwrap_used)]
                    let null = bts.iter().position(|&i| i == 0).unwrap() + 1;
                    // CStr::from_bytes_with_nul only fails if there is no 0 char or if the 0 char
                    // is not at the end of the byte array, which is not the case for bts[0..null]
                    // since we already found the first 0 (null) char position.
                    // So it's safe to use unwrap and to use index slicing.
                    #[allow(clippy::unwrap_used)]
                    #[allow(clippy::indexing_slicing)]
                    let cstr = CStr::from_bytes_with_nul(&bts[0..null]).unwrap();
                    Ok(cstr.to_str().map_err(|e| {
                        internal_err(format!("unable to create str from CStr: {}", e))
                    })?)
                } else {
                    // String is exactly eight bytes
                    Ok(std::str::from_utf8(bts).map_err(|e| {
                        internal_err(format!("unable to create str from bytes: {}", e))
                    })?)
                }
            }
            _ => Err(Self::Error::DataTypeConversionError {
                val: format!("{:?}", data),
                src_type: match data.sql_type() {
                    Some(ty) => ty.to_string(),
                    None => "Null".to_string(),
                },
                target_type: "&str".to_string(),
                details: "".to_string(),
            }),
        }
    }
}

impl TryFrom<DataType> for Vec<u8> {
    type Error = ReadySetError;

    fn try_from(data: DataType) -> Result<Self, Self::Error> {
        match data {
            DataType::Text(t) => Ok(t.to_bytes().to_vec()),
            DataType::TinyText(t) => {
                if t[TINYTEXT_WIDTH - 1] == 0 {
                    // guaranteed to have at least one 0
                    let null = t.iter().position(|&i| i == 0).unwrap();
                    Ok(t[0..null].to_vec())
                } else {
                    // entire [u8] contains data
                    Ok(t.to_vec())
                }
            }
            _ => Err(Self::Error::DataTypeConversionError {
                val: format!("{:?}", data),
                src_type: match data.sql_type() {
                    Some(ty) => ty.to_string(),
                    None => "Null".to_string(),
                },
                target_type: "&str".to_string(),
                details: "".to_string(),
            }),
        }
    }
}

impl TryFrom<DataType> for i128 {
    type Error = ReadySetError;

    fn try_from(data: DataType) -> Result<Self, Self::Error> {
        <i128>::try_from(&data)
    }
}

impl TryFrom<DataType> for i64 {
    type Error = ReadySetError;

    fn try_from(data: DataType) -> Result<i64, Self::Error> {
        <i64>::try_from(&data)
    }
}

impl TryFrom<&'_ DataType> for i128 {
    type Error = ReadySetError;

    fn try_from(data: &'_ DataType) -> Result<Self, Self::Error> {
        match *data {
            DataType::BigInt(s) => Ok(i128::from(s)),
            DataType::UnsignedBigInt(s) => Ok(i128::from(s)),
            DataType::Int(s) => Ok(i128::from(s)),
            DataType::UnsignedInt(s) => Ok(i128::from(s)),
            _ => Err(Self::Error::DataTypeConversionError {
                val: format!("{:?}", data),
                src_type: "DataType".to_string(),
                target_type: "i128".to_string(),
                details: "".to_string(),
            }),
        }
    }
}

impl TryFrom<&'_ DataType> for i64 {
    type Error = ReadySetError;

    fn try_from(data: &'_ DataType) -> Result<Self, Self::Error> {
        match *data {
            DataType::UnsignedBigInt(s) => {
                if s as i128 >= std::i64::MIN.into() && s as i128 <= std::i64::MAX.into() {
                    Ok(s as i64)
                } else {
                    Err(Self::Error::DataTypeConversionError {
                        val: format!("{:?}", data),
                        src_type: "DataType".to_string(),
                        target_type: "i64".to_string(),
                        details: "Out of bounds".to_string(),
                    })
                }
            }
            DataType::BigInt(s) => Ok(s),
            DataType::Int(s) => Ok(i64::from(s)),
            DataType::UnsignedInt(s) => Ok(i64::from(s)),
            _ => Err(Self::Error::DataTypeConversionError {
                val: format!("{:?}", data),
                src_type: "DataType".to_string(),
                target_type: "i64".to_string(),
                details: "".to_string(),
            }),
        }
    }
}

impl TryFrom<DataType> for u64 {
    type Error = ReadySetError;

    fn try_from(data: DataType) -> Result<Self, Self::Error> {
        <u64>::try_from(&data)
    }
}

impl TryFrom<&'_ DataType> for u64 {
    type Error = ReadySetError;

    fn try_from(data: &'_ DataType) -> Result<Self, Self::Error> {
        match *data {
            DataType::UnsignedBigInt(s) => Ok(s),
            DataType::BigInt(s) => {
                if s as i128 >= std::u64::MIN.into() && s as i128 <= std::u64::MAX.into() {
                    Ok(s as u64)
                } else {
                    Err(Self::Error::DataTypeConversionError {
                        val: format!("{:?}", data),
                        src_type: "DataType".to_string(),
                        target_type: "u64".to_string(),
                        details: "Out of bounds".to_string(),
                    })
                }
            }
            DataType::UnsignedInt(s) => Ok(u64::from(s)),
            DataType::Int(s) => {
                if s as i128 >= std::u64::MIN.into() && s as i128 <= std::u64::MAX.into() {
                    Ok(s as u64)
                } else {
                    Err(Self::Error::DataTypeConversionError {
                        val: format!("{:?}", data),
                        src_type: "DataType".to_string(),
                        target_type: "u64".to_string(),
                        details: "Out of bounds".to_string(),
                    })
                }
            }
            _ => Err(Self::Error::DataTypeConversionError {
                val: format!("{:?}", data),
                src_type: "DataType".to_string(),
                target_type: "u64".to_string(),
                details: "".to_string(),
            }),
        }
    }
}

impl TryFrom<DataType> for i32 {
    type Error = ReadySetError;

    fn try_from(data: DataType) -> Result<Self, Self::Error> {
        <i32>::try_from(&data)
    }
}

impl TryFrom<&'_ DataType> for i32 {
    type Error = ReadySetError;

    fn try_from(data: &'_ DataType) -> Result<Self, Self::Error> {
        match *data {
            DataType::UnsignedBigInt(s) => {
                if s as i128 >= std::i32::MIN.into() && s as i128 <= std::i32::MAX.into() {
                    Ok(s as i32)
                } else {
                    Err(Self::Error::DataTypeConversionError {
                        val: format!("{:?}", data),
                        src_type: "DataType".to_string(),
                        target_type: "i32".to_string(),
                        details: "out of bounds".to_string(),
                    })
                }
            }
            DataType::BigInt(s) => {
                if s as i128 >= std::i32::MIN.into() && s as i128 <= std::i32::MAX.into() {
                    Ok(s as i32)
                } else {
                    Err(Self::Error::DataTypeConversionError {
                        val: format!("{:?}", data),
                        src_type: "DataType".to_string(),
                        target_type: "i32".to_string(),
                        details: "out of bounds".to_string(),
                    })
                }
            }
            DataType::UnsignedInt(s) => {
                if s as i128 >= std::i32::MIN.into() && s as i128 <= std::i32::MAX.into() {
                    Ok(s as i32)
                } else {
                    Err(Self::Error::DataTypeConversionError {
                        val: format!("{:?}", data),
                        src_type: "DataType".to_string(),
                        target_type: "i32".to_string(),
                        details: "out of bounds".to_string(),
                    })
                }
            }
            DataType::Int(s) => Ok(s),
            _ => Err(Self::Error::DataTypeConversionError {
                val: format!("{:?}", data),
                src_type: "DataType".to_string(),
                target_type: "i32".to_string(),
                details: "".to_string(),
            }),
        }
    }
}

impl TryFrom<DataType> for u32 {
    type Error = ReadySetError;
    fn try_from(data: DataType) -> Result<Self, Self::Error> {
        <u32>::try_from(&data)
    }
}

impl TryFrom<&'_ DataType> for u32 {
    type Error = ReadySetError;

    fn try_from(data: &'_ DataType) -> Result<Self, Self::Error> {
        match *data {
            DataType::UnsignedBigInt(s) => {
                if s as i128 >= std::u32::MIN.into() && s as i128 <= std::u32::MAX.into() {
                    Ok(s as u32)
                } else {
                    Err(Self::Error::DataTypeConversionError {
                        val: format!("{:?}", data),
                        src_type: "DataType".to_string(),
                        target_type: "u32".to_string(),
                        details: "out of bounds".to_string(),
                    })
                }
            }
            DataType::BigInt(s) => {
                if s as i128 >= std::u32::MIN.into() && s as i128 <= std::u32::MAX.into() {
                    Ok(s as u32)
                } else {
                    Err(Self::Error::DataTypeConversionError {
                        val: format!("{:?}", data),
                        src_type: "DataType".to_string(),
                        target_type: "u32".to_string(),
                        details: "out of bounds".to_string(),
                    })
                }
            }
            DataType::UnsignedInt(s) => Ok(s),
            DataType::Int(s) => {
                if s as i128 >= std::u32::MIN.into() && s as i128 <= std::u32::MAX.into() {
                    Ok(s as u32)
                } else {
                    Err(Self::Error::DataTypeConversionError {
                        val: format!("{:?}", data),
                        src_type: "DataType".to_string(),
                        target_type: "u32".to_string(),
                        details: "out of bounds".to_string(),
                    })
                }
            }
            _ => Err(Self::Error::DataTypeConversionError {
                val: format!("{:?}", data),
                src_type: "DataType".to_string(),
                target_type: "u32".to_string(),
                details: "".to_string(),
            }),
        }
    }
}

impl TryFrom<DataType> for f32 {
    type Error = ReadySetError;

    fn try_from(data: DataType) -> Result<Self, Self::Error> {
        <f32>::try_from(&data)
    }
}

impl TryFrom<&'_ DataType> for f32 {
    type Error = ReadySetError;

    fn try_from(data: &'_ DataType) -> Result<Self, Self::Error> {
        match *data {
            DataType::Float(f, _) => Ok(f),
            DataType::Double(f, _) => Ok(f as f32),
            DataType::UnsignedInt(i) => Ok(i as f32),
            DataType::Int(i) => Ok(i as f32),
            DataType::UnsignedBigInt(i) => Ok(i as f32),
            DataType::BigInt(i) => Ok(i as f32),
            _ => Err(Self::Error::DataTypeConversionError {
                val: format!("{:?}", data),
                src_type: "DataType".to_string(),
                target_type: "f32".to_string(),
                details: "".to_string(),
            }),
        }
    }
}

impl TryFrom<DataType> for f64 {
    type Error = ReadySetError;

    fn try_from(data: DataType) -> Result<Self, Self::Error> {
        <f64>::try_from(&data)
    }
}

impl TryFrom<&'_ DataType> for f64 {
    type Error = ReadySetError;

    fn try_from(data: &'_ DataType) -> Result<Self, Self::Error> {
        match *data {
            DataType::Float(f, _) => Ok(f as f64),
            DataType::Double(f, _) => Ok(f),
            DataType::UnsignedInt(i) => Ok(f64::from(i)),
            DataType::Int(i) => Ok(f64::from(i)),
            DataType::UnsignedBigInt(i) => Ok(i as f64),
            DataType::BigInt(i) => Ok(i as f64),
            _ => Err(Self::Error::DataTypeConversionError {
                val: format!("{:?}", data),
                src_type: "DataType".to_string(),
                target_type: "f64".to_string(),
                details: "".to_string(),
            }),
        }
    }
}

impl From<String> for DataType {
    fn from(s: String) -> Self {
        #[allow(clippy::unwrap_used)]
        // The only way for this to fail is if the `String` is not a valid UTF-8 String,
        // which can never happen since all Strings in Rust are valid UTF-8 encoded.
        DataType::try_from(s.as_bytes()).unwrap()
    }
}

impl TryFrom<&'_ DataType> for String {
    type Error = ReadySetError;

    fn try_from(dt: &DataType) -> Result<Self, Self::Error> {
        let s: &str = <&str>::try_from(dt)?;
        Ok(s.into())
    }
}

impl TryFrom<DataType> for String {
    type Error = ReadySetError;

    fn try_from(dt: DataType) -> Result<Self, Self::Error> {
        String::try_from(&dt)
    }
}

impl<'a> From<&'a str> for DataType {
    fn from(s: &'a str) -> Self {
        #[allow(clippy::unwrap_used)]
        // The only way for this to fail is if the `&str` is not a valid UTF-8 string,
        // which can never happen since all String slices in Rust are valid UTF-8 encoded.
        DataType::try_from(s.as_bytes()).unwrap()
    }
}

impl<'a> TryFrom<&'a [u8]> for DataType {
    type Error = ReadySetError;

    fn try_from(b: &[u8]) -> Result<Self, Self::Error> {
        let len = b.len();
        if len <= TINYTEXT_WIDTH {
            let mut bytes = [0; TINYTEXT_WIDTH];
            if len != 0 {
                // We know at this point that `len` is less or equal to `TINYTEXT_WIDTH`,
                // which is the size of the `bytes` array.
                #[allow(clippy::indexing_slicing)]
                let bts = &mut bytes[0..len];
                bts.copy_from_slice(b);
            }

            Ok(DataType::TinyText(bytes))
        } else {
            match ArcCStr::try_from(b) {
                Ok(arc_c_str) => Ok(DataType::Text(arc_c_str)),
                Err(_) => Err(Self::Error::DataTypeConversionError {
                    val: format!("{:?}", b),
                    src_type: "DataType".to_string(),
                    target_type: "&[u8]".to_string(),
                    details: "".to_string(),
                }),
            }
        }
    }
}

impl TryFrom<mysql_common::value::Value> for DataType {
    type Error = ReadySetError;

    fn try_from(v: mysql_common::value::Value) -> Result<Self, Self::Error> {
        DataType::try_from(&v)
    }
}

impl TryFrom<&mysql_common::value::Value> for DataType {
    type Error = ReadySetError;

    fn try_from(v: &mysql_common::value::Value) -> Result<Self, Self::Error> {
        use mysql_common::value::Value;

        match v {
            Value::NULL => Ok(DataType::None),
            Value::Bytes(v) => DataType::try_from(&v[..]),
            Value::Int(v) => Ok(DataType::from(*v)),
            Value::UInt(v) => Ok(DataType::from(*v)),
            Value::Float(v) => DataType::try_from(*v),
            Value::Double(v) => DataType::try_from(*v),
            Value::Date(year, month, day, hour, minutes, seconds, micros) => {
                Ok(DataType::Timestamp(
                    NaiveDate::from_ymd((*year).into(), (*month).into(), (*day).into())
                        .and_hms_micro(
                            (*hour).into(),
                            (*minutes).into(),
                            (*seconds).into(),
                            *micros,
                        ),
                ))
            }
            Value::Time(neg, days, hours, minutes, seconds, microseconds) => {
                Ok(DataType::Time(Arc::new(MysqlTime::from_hmsus(
                    !neg,
                    <u16>::try_from(*hours as u32 + days * 24u32).unwrap_or(u16::MAX),
                    *minutes,
                    *seconds,
                    (*microseconds).into(),
                ))))
            }
        }
    }
}

impl ToSql for DataType {
    fn to_sql(
        &self,
        ty: &Type,
        out: &mut BytesMut,
    ) -> Result<IsNull, Box<dyn Error + 'static + Sync + Send>> {
        match self {
            Self::None => None::<i8>.to_sql(ty, out),
            Self::Int(x) => x.to_sql(ty, out),
            Self::UnsignedInt(x) if *ty == Type::BOOL => (*x != 0).to_sql(ty, out),
            Self::UnsignedInt(x) => x.to_sql(ty, out),
            Self::BigInt(x) => x.to_sql(ty, out),
            Self::UnsignedBigInt(x) => (*x as i64).to_sql(ty, out),
            Self::Float(x, _) => x.to_sql(ty, out),
            Self::Double(x, _) => x.to_sql(ty, out),
            Self::Text(_) | Self::TinyText(_) => <&str>::try_from(self).unwrap().to_sql(ty, out),
            Self::Timestamp(x) => x.to_sql(ty, out),
            Self::Time(x) => NaiveTime::from(**x).to_sql(ty, out),
        }
    }

    accepts!(
        BOOL, BYTEA, CHAR, NAME, INT2, INT4, INT8, TEXT, VARCHAR, DATE, TIME, TIMESTAMP, FLOAT4,
        FLOAT8
    );

    to_sql_checked!();
}

impl TryFrom<DataType> for mysql_common::value::Value {
    type Error = ReadySetError;

    fn try_from(dt: DataType) -> Result<Self, Self::Error> {
        use mysql_common::value::Value;

        match dt {
            DataType::None => Ok(Value::NULL),
            DataType::Int(val) => Ok(Value::Int(i64::from(val))),
            DataType::UnsignedInt(val) => Ok(Value::UInt(u64::from(val))),
            DataType::BigInt(val) => Ok(Value::Int(val)),
            DataType::UnsignedBigInt(val) => Ok(Value::UInt(val)),
            DataType::Float(val, _) => Ok(Value::Float(val)),
            DataType::Double(val, _) => Ok(Value::Double(val)),
            DataType::Text(_) => Ok(Value::Bytes(Vec::<u8>::try_from(dt)?)),
            DataType::TinyText(_) => Ok(Value::Bytes(Vec::<u8>::try_from(dt)?)),
            DataType::Timestamp(val) => Ok(val.into()),
            DataType::Time(val) => Ok(Value::Time(
                !val.is_positive(),
                (val.hour() / 24).into(),
                (val.hour() % 24) as _,
                val.minutes(),
                val.seconds(),
                val.microseconds(),
            )),
        }
    }
}

// Performs an arithmetic operation on two numeric DataTypes,
// returning a new DataType as the result.
macro_rules! arithmetic_operation (
    ($op:tt, $first:ident, $second:ident) => (
        match ($first, $second) {
            (&DataType::None, _) | (_, &DataType::None) => DataType::None,
            (&DataType::Int(a), &DataType::Int(b)) => (a $op b).into(),
            (&DataType::UnsignedInt(a), &DataType::UnsignedInt(b)) => (a $op b).into(),
            (&DataType::BigInt(a), &DataType::BigInt(b)) => (a $op b).into(),
            (&DataType::UnsignedBigInt(a), &DataType::UnsignedBigInt(b)) => (a $op b).into(),

            (&DataType::Int(a), &DataType::BigInt(b)) => (i64::from(a) $op b).into(),
            (&DataType::BigInt(a), &DataType::Int(b)) => (a $op i64::from(b)).into(),
            (&DataType::Int(a), &DataType::UnsignedBigInt(b)) => DataType::try_from(i128::from(a) $op i128::from(b))?,
            (&DataType::UnsignedBigInt(a), &DataType::Int(b)) => DataType::try_from(i128::from(a) $op i128::from(b))?,
            (&DataType::BigInt(a), &DataType::UnsignedBigInt(b)) => DataType::try_from(i128::from(a) $op i128::from(b))?,
            (&DataType::UnsignedBigInt(a), &DataType::BigInt(b)) => DataType::try_from(i128::from(a) $op i128::from(b))?,
            (&DataType::UnsignedBigInt(a), &DataType::UnsignedInt(b)) => (a $op u64::from(b)).into(),
            (&DataType::UnsignedInt(a), &DataType::UnsignedBigInt(b)) => (u64::from(a) $op b).into(),

            (first @ &DataType::Int(..), second @ &DataType::Float(..)) |
            (first @ &DataType::BigInt(..), second @ &DataType::Float(..)) |
            (first @ &DataType::UnsignedInt(..), second @ &DataType::Float(..)) |
            (first @ &DataType::UnsignedBigInt(..), second @ &DataType::Float(..)) |
            (first @ &DataType::Float(..), second @ &DataType::Int(..)) |
            (first @ &DataType::Float(..), second @ &DataType::BigInt(..)) |
            (first @ &DataType::Float(..), second @ &DataType::UnsignedInt(..)) |
            (first @ &DataType::Float(..), second @ &DataType::UnsignedBigInt(..)) |
            (first @ &DataType::Float(..), second @ &DataType::Float(..)) |
            (first @ &DataType::Float(..), second @ &DataType::Double(..)) => {
                let a: f32 = f32::try_from(first)?;
                let b: f32 = f32::try_from(second)?;
                DataType::try_from(a $op b)?
            }

            (first @ &DataType::Int(..), second @ &DataType::Double(..)) |
            (first @ &DataType::BigInt(..), second @ &DataType::Double(..)) |
            (first @ &DataType::UnsignedInt(..), second @ &DataType::Double(..)) |
            (first @ &DataType::UnsignedBigInt(..), second @ &DataType::Double(..)) |
            (first @ &DataType::Double(..), second @ &DataType::Int(..)) |
            (first @ &DataType::Double(..), second @ &DataType::BigInt(..)) |
            (first @ &DataType::Double(..), second @ &DataType::UnsignedInt(..)) |
            (first @ &DataType::Double(..), second @ &DataType::UnsignedBigInt(..)) |
            (first @ &DataType::Double(..), second @ &DataType::Double(..)) |
            (first @ &DataType::Double(..), second @ &DataType::Float(..)) => {
                let a: f64 = f64::try_from(first)?;
                let b: f64 = f64::try_from(second)?;
                DataType::try_from(a $op b)?
            }
            (first, second) => panic!(
                "can't {} a {:?} and {:?}",
                stringify!($op),
                first,
                second,
            ),
        }
    );
);

impl<'a, 'b> Add<&'b DataType> for &'a DataType {
    type Output = ReadySetResult<DataType>;

    fn add(self, other: &'b DataType) -> Self::Output {
        Ok(arithmetic_operation!(+, self, other))
    }
}

impl<'a, 'b> Sub<&'b DataType> for &'a DataType {
    type Output = ReadySetResult<DataType>;

    fn sub(self, other: &'b DataType) -> Self::Output {
        Ok(arithmetic_operation!(-, self, other))
    }
}

impl<'a, 'b> Mul<&'b DataType> for &'a DataType {
    type Output = ReadySetResult<DataType>;

    fn mul(self, other: &'b DataType) -> Self::Output {
        Ok(arithmetic_operation!(*, self, other))
    }
}

impl<'a, 'b> Div<&'b DataType> for &'a DataType {
    type Output = ReadySetResult<DataType>;

    fn div(self, other: &'b DataType) -> Self::Output {
        Ok(arithmetic_operation!(/, self, other))
    }
}

/// A modification to make to an existing value.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum Operation {
    /// Add the given value to the existing one.
    Add,
    /// Subtract the given value from the existing value.
    Sub,
}

/// A modification to make to a column in an existing row.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum Modification {
    /// Set the cell to this value.
    Set(DataType),
    /// Use the given [`Operation`] to combine the existing value and this one.
    Apply(Operation, DataType),
    /// Leave the existing value as-is.
    None,
}

impl<T> From<T> for Modification
where
    T: Into<DataType>,
{
    fn from(t: T) -> Modification {
        Modification::Set(t.into())
    }
}

/// A data type representing an offset in a replication log
///
/// Replication offsets are represented by a single global [offset](ReplicationOffset::offset),
/// scoped within a single log, identified by a
/// [`replication_log_name`](ReplicationOffset::replication_log_name). Within a single log, offsets
/// are totally ordered, but outside the scope of a log ordering is not well-defined.
///
/// See [the documentation for PersistentState](::noria_dataflow::state::persistent_state) for
/// more information about how replication offsets are used and persisted
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct ReplicationOffset {
    /// The actual offset within the replication log
    pub offset: u128,

    /// The name of the replication log that this offset is within. [`ReplicationOffset`]s with
    /// different log names are not comparable
    pub replication_log_name: String,
}

impl PartialOrd for ReplicationOffset {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        if other.replication_log_name != self.replication_log_name {
            None
        } else {
            self.offset.partial_cmp(&other.offset)
        }
    }
}

impl ReplicationOffset {
    /// Try to mutate `other` to take the maximum of its offset and the offset of
    /// `self`. If `other` is `None`, will assign it to `Some(self.clone)`.
    ///
    /// If the offsets are from different replication logs, returns an error with
    /// [`ReadySetError::ReplicationOffsetLogDifferent`]
    pub fn try_max_into(&self, other: &mut Option<ReplicationOffset>) -> ReadySetResult<()> {
        if let Some(other) = other {
            if self.replication_log_name != other.replication_log_name {
                return Err(ReadySetError::ReplicationOffsetLogDifferent(
                    self.replication_log_name.clone(),
                    other.replication_log_name.clone(),
                ));
            }

            if self.offset > other.offset {
                other.offset = self.offset
            }
        } else {
            *other = Some(self.clone())
        }

        Ok(())
    }
}

/// An operation to apply to a base table.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum TableOperation {
    /// Insert the contained row.
    Insert(Vec<DataType>),
    /// Delete a row with the contained key.
    DeleteByKey {
        /// The key.
        key: Vec<DataType>,
    },
    /// Delete *one* row matching the entirety of the given row
    DeleteRow {
        /// The row to delete
        row: Vec<DataType>,
    },
    /// If a row exists with the same key as the contained row, update it using `update`, otherwise
    /// insert `row`.
    InsertOrUpdate {
        /// This row will be inserted if no existing row is found.
        row: Vec<DataType>,
        /// These modifications will be applied to the columns of an existing row.
        update: Vec<Modification>,
    },
    /// Update an existing row with the given `key`.
    Update {
        /// The modifications to make to each column of the existing row.
        update: Vec<Modification>,
        /// The key used to identify the row to update.
        key: Vec<DataType>,
    },
    /// Set the replication offset for data written to this base table.
    ///
    /// Within a group of table operations, the largest replication offset will take precedence
    ///
    /// See [the documentation for PersistentState](::noria_dataflow::state::persistent_state) for
    /// more information about replication offsets.
    SetReplicationOffset(ReplicationOffset),
}

impl TableOperation {
    #[doc(hidden)]
    pub fn row(&self) -> Option<&[DataType]> {
        match *self {
            TableOperation::Insert(ref r) => Some(r),
            TableOperation::InsertOrUpdate { ref row, .. } => Some(row),
            _ => None,
        }
    }

    /// Construct an iterator over the shards this TableOperation should target.
    ///
    /// ## Invariants
    /// * `key_col` must be in the rows.
    /// * the `key`s must have at least one element.
    #[inline]
    pub fn shards(&self, key_col: usize, num_shards: usize) -> impl Iterator<Item = usize> {
        #[allow(clippy::indexing_slicing)]
        let key = match self {
            TableOperation::Insert(row) => Some(&row[key_col]),
            TableOperation::DeleteByKey { key } => Some(&key[0]),
            TableOperation::DeleteRow { row } => Some(&row[key_col]),
            TableOperation::Update { key, .. } => Some(&key[0]),
            TableOperation::InsertOrUpdate { row, .. } => Some(&row[key_col]),
            TableOperation::SetReplicationOffset(_) => None,
        };

        if let Some(key) = key {
            Either::Left(iter::once(crate::shard_by(key, num_shards)))
        } else {
            // updates to replication offsets should hit all shards
            Either::Right(0..num_shards)
        }
    }
}

impl From<Vec<DataType>> for TableOperation {
    fn from(other: Vec<DataType>) -> Self {
        TableOperation::Insert(other)
    }
}

impl Arbitrary for DataType {
    type Parameters = ();
    type Strategy = proptest::strategy::BoxedStrategy<DataType>;

    fn arbitrary_with(_args: Self::Parameters) -> Self::Strategy {
        use launchpad::arbitrary::{arbitrary_duration, arbitrary_naive_date_time};
        use proptest::arbitrary::any;
        use proptest::prelude::*;
        use DataType::*;

        prop_oneof![
            Just(None),
            any::<i32>().prop_map(Int),
            any::<u32>().prop_map(UnsignedInt),
            any::<i64>().prop_map(BigInt),
            any::<u64>().prop_map(UnsignedBigInt),
            any::<(f32, u8)>().prop_map(|(f, p)| Float(f, p)),
            any::<(f64, u8)>().prop_map(|(f, p)| Double(f, p)),
            any::<String>().prop_map(|s| DataType::from(s.replace("\0", ""))),
            arbitrary_naive_date_time().prop_map(Timestamp),
            arbitrary_duration()
                .prop_map(MysqlTime::new)
                .prop_map(Arc::new)
                .prop_map(Time),
        ]
        .boxed()
    }
}

#[derive(Debug, From, Into)]
struct MySqlValue(mysql_common::value::Value);

impl Arbitrary for MySqlValue {
    type Parameters = ();
    type Strategy = proptest::strategy::BoxedStrategy<MySqlValue>;

    fn arbitrary_with(_args: Self::Parameters) -> Self::Strategy {
        use mysql_common::value::Value;
        use proptest::arbitrary::any;
        use proptest::prelude::*;

        //TODO(DAN): cleaner way of avoiding invalid dates/times
        prop_oneof![
            Just(Value::NULL),
            any::<i64>().prop_map(Value::Int),
            any::<u64>().prop_map(Value::UInt),
            any::<f32>().prop_map(Value::Float),
            any::<f64>().prop_map(Value::Double),
            (
                1001u16..9999,
                1u8..13,
                1u8..29, //TODO(DAN): Allow up to 31 and check for invalid dates in the test
                0u8..24,
                0u8..60,
                0u8..60,
                0u32..100
            )
                .prop_map(|(y, m, d, h, min, s, ms)| Value::Date(y, m, d, h, min, s, ms)),
            (
                any::<bool>(),
                0u32..34, // DataType cannot accept time hihger than 838:59:59
                0u8..24,
                0u8..60,
                0u8..60,
                0u32..100
            )
                .prop_map(|(neg, d, h, m, s, ms)| Value::Time(neg, d, h, m, s, ms)),
        ]
        .prop_map(MySqlValue)
        .boxed()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use test_strategy::proptest;

    #[proptest]
    #[allow(clippy::float_cmp)]
    fn dt_to_mysql_value_roundtrip_prop(v: MySqlValue) {
        use mysql_common::value::Value;
        let MySqlValue(v) = v;
        match (
            Value::try_from(DataType::try_from(v.clone()).unwrap()).unwrap(),
            v,
        ) {
            (Value::Float(f1), Value::Float(f2)) => assert_eq!(f1, f2),
            (Value::Double(d1), Value::Double(d2)) => assert_eq!(d1, d2),
            (v1, v2) => assert_eq!(v1, v2),
        }
    }

    #[proptest]
    #[allow(clippy::float_cmp)]
    fn mysql_value_to_dt_roundtrip_prop(dt: DataType) {
        use chrono::Datelike;
        use mysql_common::value::Value;

        prop_assume!(!matches!(
            dt,
            DataType::Timestamp(t)
                if t.date().year() < 1000 ||
                   t.date().year() > 9999
        ));

        match (
            DataType::try_from(Value::try_from(dt.clone()).unwrap()).unwrap(),
            dt,
        ) {
            (DataType::Float(f1, _), DataType::Float(f2, _)) => assert_eq!(f1, f2),
            (DataType::Double(f1, _), DataType::Double(f2, _)) => assert_eq!(f1, f2),
            (dt1, dt2) => assert_eq!(dt1, dt2),
        }
    }

    #[test]
    fn mysql_value_to_datatype_roundtrip() {
        use mysql_common::value::Value;

        assert_eq!(
            Value::Bytes(vec![1, 2, 3]),
            Value::try_from(DataType::try_from(Value::Bytes(vec![1, 2, 3])).unwrap()).unwrap()
        );

        assert_eq!(
            Value::Int(1),
            Value::try_from(DataType::try_from(Value::Int(1)).unwrap()).unwrap()
        );
        assert_eq!(
            Value::UInt(1),
            Value::try_from(DataType::try_from(Value::UInt(1)).unwrap()).unwrap()
        );
        // round trip results in conversion from float to double
        assert_eq!(
            Value::Float(8.99),
            Value::try_from(DataType::try_from(Value::Float(8.99)).unwrap()).unwrap()
        );
        assert_eq!(
            Value::Double(8.99),
            Value::try_from(DataType::try_from(Value::Double(8.99)).unwrap()).unwrap()
        );
        assert_eq!(
            Value::Date(2021, 1, 1, 1, 1, 1, 1),
            Value::try_from(DataType::try_from(Value::Date(2021, 1, 1, 1, 1, 1, 1)).unwrap())
                .unwrap()
        );
        assert_eq!(
            Value::Time(false, 1, 1, 1, 1, 1),
            Value::try_from(DataType::try_from(Value::Time(false, 1, 1, 1, 1, 1)).unwrap())
                .unwrap()
        );
    }

    #[test]
    #[allow(clippy::float_cmp)]
    fn mysql_value_to_datatype() {
        use mysql_common::value::Value;

        // Test Value::NULL.
        let a = Value::NULL;
        let a_dt = DataType::try_from(a);
        assert!(a_dt.is_ok());
        assert_eq!(a_dt.unwrap(), DataType::None);

        // Test Value::Bytes.
        // Can't build a CString with interior nul-terminated chars.
        let a = Value::Bytes(vec![0; 30]);
        let a_dt = DataType::try_from(a);
        assert!(a_dt.is_err());

        let a = Value::Bytes(vec![1; 30]);
        let a_dt = DataType::try_from(a);
        assert!(a_dt.is_ok());
        assert_eq!(
            a_dt.unwrap(),
            DataType::Text(ArcCStr::try_from(&vec![1; 30][..]).unwrap())
        );

        let s = [1; 15];
        let a = Value::Bytes(s.to_vec());
        let a_dt = DataType::try_from(a);
        assert!(a_dt.is_ok());
        assert_eq!(a_dt.unwrap(), DataType::TinyText(s));

        // Test Value::Int.
        let a = Value::Int(-5);
        let a_dt = DataType::try_from(a);
        assert!(a_dt.is_ok());
        assert_eq!(a_dt.unwrap(), DataType::BigInt(-5));

        // Test Value::Float.
        let a = Value::UInt(5);
        let a_dt = DataType::try_from(a);
        assert!(a_dt.is_ok());
        assert_eq!(a_dt.unwrap(), DataType::UnsignedBigInt(5));

        // Test Value::Float.
        let initial_float: f32 = 8.99;
        let a = Value::Float(initial_float);
        let a_dt = DataType::try_from(a);
        assert!(a_dt.is_ok());
        let converted_float: f32 = <f32>::try_from(a_dt.unwrap()).unwrap();
        assert_eq!(converted_float, initial_float);

        // Test Value::Double.
        let initial_float: f64 = 8.99;
        let a = Value::Double(initial_float);
        let a_dt = DataType::try_from(a);
        assert!(a_dt.is_ok());
        let converted_float: f64 = <f64>::try_from(a_dt.unwrap()).unwrap();
        assert_eq!(converted_float, initial_float);

        // Test Value::Date.
        let ts = NaiveDate::from_ymd(1111, 1, 11).and_hms_micro(2, 3, 4, 5);
        let a = Value::from(ts);
        let a_dt = DataType::try_from(a);
        assert!(a_dt.is_ok());
        assert_eq!(a_dt.unwrap(), DataType::Timestamp(ts));

        // Test Value::Time.
        // noria::DataType has no `Time` representation.
        let a = Value::Time(true, 0, 0, 0, 0, 0);
        let a_dt = DataType::try_from(a);
        assert!(a_dt.is_ok());
        assert_eq!(
            a_dt.unwrap(),
            DataType::Time(Arc::new(MysqlTime::from_microseconds(0)))
        )
    }

    #[test]
    fn real_to_string() {
        let a_float: DataType = DataType::try_from(8.99_f32).unwrap();
        let b_float: DataType = DataType::try_from(-8.099_f32).unwrap();
        let c_float: DataType = DataType::try_from(-0.012_345_678_f32).unwrap();
        assert_eq!(a_float.to_string(), "8.99");
        assert_eq!(b_float.to_string(), "-8.099");
        assert_eq!(c_float.to_string(), "-0.012345678");

        let a_double: DataType = DataType::try_from(8.99_f64).unwrap();
        let b_double: DataType = DataType::try_from(-8.099_f64).unwrap();
        let c_double: DataType = DataType::try_from(-0.012_345_678_f64).unwrap();
        assert_eq!(a_double.to_string(), "8.99");
        assert_eq!(b_double.to_string(), "-8.099");
        assert_eq!(c_double.to_string(), "-0.012345678");
    }

    #[test]
    #[allow(clippy::float_cmp)]
    fn real_to_float() {
        let original: f32 = 8.99;
        let data_type: DataType = DataType::try_from(original).unwrap();
        let converted: f32 = <f32>::try_from(&data_type).unwrap();
        assert_eq!(DataType::Float(8.99, u8::MAX), data_type);
        assert_eq!(original, converted);
    }

    #[test]
    #[allow(clippy::float_cmp)]
    fn real_to_double() {
        let original: f64 = 8.99;
        let data_type: DataType = DataType::try_from(original).unwrap();
        let converted: f64 = <f64>::try_from(&data_type).unwrap();
        assert_eq!(DataType::Double(8.99, u8::MAX), data_type);
        assert_eq!(original, converted);
    }

    macro_rules! assert_arithmetic {
        ($left:literal $op:tt $right:literal = $expected:literal) => {
            assert_eq!(
                (&DataType::try_from($left).unwrap() $op &DataType::try_from($right).unwrap()).unwrap(),
                DataType::try_from($expected).unwrap()
            );
        }
    }

    #[test]
    fn add_data_types() {
        assert_arithmetic!(1 + 2 = 3);
        assert_arithmetic!(1.5_f32 + 2 = 3.5_f32);
        assert_arithmetic!(1.5_f64 + 2 = 3.5_f64);
        assert_arithmetic!(2 + 1.5_f32 = 3.5_f32);
        assert_arithmetic!(2 + 1.5_f64 = 3.5_f64);
        assert_arithmetic!(1.5_f32 + 2.5_f32 = 4.0_f32);
        assert_arithmetic!(1.5_f64 + 2.5_f64 = 4.0_f64);
        assert_arithmetic!(1.5_f32 + 2.5_f64 = 4.0_f64);
        assert_arithmetic!(1.5_f64 + 2.5_f32 = 4.0_f64);
        assert_eq!(
            (&DataType::BigInt(1) + &DataType::BigInt(2)).unwrap(),
            3.into()
        );
        assert_eq!(
            (&DataType::from(1) + &DataType::BigInt(2)).unwrap(),
            3.into()
        );
        assert_eq!(
            (&DataType::BigInt(2) + &DataType::try_from(1).unwrap()).unwrap(),
            3.into()
        );
    }

    #[test]
    fn subtract_data_types() {
        assert_arithmetic!(2 - 1 = 1);
        assert_arithmetic!(3.5_f32 - 2 = 1.5_f32);
        assert_arithmetic!(3.5_f64 - 2 = 1.5_f64);
        assert_arithmetic!(2 - 1.5_f32 = 0.5_f32);
        assert_arithmetic!(2 - 1.5_f64 = 0.5_f64);
        assert_arithmetic!(3.5_f32 - 2.0_f32 = 1.5_f32);
        assert_arithmetic!(3.5_f64 - 2.0_f64 = 1.5_f64);
        assert_arithmetic!(3.5_f32 - 2.0_f64 = 1.5_f64);
        assert_arithmetic!(3.5_f64 - 2.0_f32 = 1.5_f64);
        assert_eq!(
            (&DataType::BigInt(1) - &DataType::BigInt(2)).unwrap(),
            (-1).into()
        );
        assert_eq!(
            (&DataType::from(1) - &DataType::BigInt(2)).unwrap(),
            (-1).into()
        );
        assert_eq!(
            (&DataType::BigInt(2) - &DataType::from(1)).unwrap(),
            1.into()
        );
    }

    #[test]
    fn multiply_data_types() {
        assert_arithmetic!(2 * 1 = 2);
        assert_arithmetic!(3.5_f32 * 2 = 7.0_f32);
        assert_arithmetic!(3.5_f64 * 2 = 7.0_f64);
        assert_arithmetic!(2 * 1.5_f32 = 3.0_f32);
        assert_arithmetic!(2 * 1.5_f64 = 3.0_f64);
        assert_arithmetic!(3.5_f32 * 2.0_f32 = 7.0_f32);
        assert_arithmetic!(3.5_f64 * 2.0_f64 = 7.0_f64);
        assert_arithmetic!(3.5_f32 * 2.0_f64 = 7.0_f64);
        assert_arithmetic!(3.5_f64 * 2.0_f32 = 7.0_f64);
        assert_eq!(
            (&DataType::BigInt(1) * &DataType::BigInt(2)).unwrap(),
            2.into()
        );
        assert_eq!(
            (&DataType::from(1) * &DataType::BigInt(2)).unwrap(),
            2.into()
        );
        assert_eq!(
            (&DataType::BigInt(2) * &DataType::from(1)).unwrap(),
            2.into()
        );
    }

    #[test]
    fn divide_data_types() {
        assert_arithmetic!(2 / 1 = 2);
        assert_arithmetic!(7.5_f32 / 2 = 3.75_f32);
        assert_arithmetic!(7.5_f64 / 2 = 3.75_f64);
        assert_arithmetic!(7 / 2.5_f32 = 2.8_f32);
        assert_arithmetic!(7 / 2.5_f64 = 2.8_f64);
        assert_arithmetic!(3.5_f32 / 2.0_f32 = 1.75_f32);
        assert_arithmetic!(3.5_f64 / 2.0_f64 = 1.75_f64);
        assert_arithmetic!(3.5_f32 / 2.0_f64 = 1.75_f64);
        assert_arithmetic!(3.5_f64 / 2.0_f32 = 1.75_f64);
        assert_eq!(
            (&DataType::BigInt(4) / &DataType::BigInt(2)).unwrap(),
            2.into()
        );
        assert_eq!(
            (&DataType::from(4) / &DataType::BigInt(2)).unwrap(),
            2.into()
        );
        assert_eq!(
            (&DataType::BigInt(4) / &DataType::from(2)).unwrap(),
            2.into()
        );
    }

    #[test]
    #[should_panic(expected = "can't + a TinyText(\"hi\") and Int(5)")]
    fn add_invalid_types() {
        let a: DataType = "hi".try_into().unwrap();
        let b: DataType = 5.into();
        let _ = &a + &b;
    }

    #[test]
    fn data_type_debug() {
        let tiny_text: DataType = "hi".try_into().unwrap();
        let text: DataType = "I contain ' and \"".try_into().unwrap();
        let float_from_real: DataType = DataType::try_from(-0.05_f32).unwrap();
        let float = DataType::Float(-0.05, 3);
        let double_from_real: DataType = DataType::try_from(-0.05_f64).unwrap();
        let double = DataType::Double(-0.05, 3);
        let timestamp = DataType::Timestamp(NaiveDateTime::from_timestamp(0, 42_000_000));
        let int = DataType::Int(5);
        let big_int = DataType::BigInt(5);
        assert_eq!(format!("{:?}", tiny_text), "TinyText(\"hi\")");
        assert_eq!(format!("{:?}", text), "Text(\"I contain ' and \\\"\")");
        assert_eq!(format!("{:?}", float_from_real), "Float(-0.05, 255)");
        assert_eq!(format!("{:?}", float), "Float(-0.05, 3)");
        assert_eq!(format!("{:?}", double_from_real), "Double(-0.05, 255)");
        assert_eq!(format!("{:?}", double), "Double(-0.05, 3)");
        assert_eq!(
            format!("{:?}", timestamp),
            "Timestamp(1970-01-01T00:00:00.042)"
        );
        assert_eq!(format!("{:?}", int), "Int(5)");
        assert_eq!(format!("{:?}", big_int), "BigInt(5)");
    }

    #[test]
    fn data_type_display() {
        let tiny_text: DataType = "hi".try_into().unwrap();
        let text: DataType = "this is a very long text indeed".try_into().unwrap();
        let float_from_real: DataType = DataType::try_from(-8.99_f32).unwrap();
        let float = DataType::Float(-8.99, 3);
        let double_from_real: DataType = DataType::try_from(-8.99_f64).unwrap();
        let double = DataType::Double(-8.99, 3);
        let timestamp = DataType::Timestamp(NaiveDateTime::from_timestamp(0, 42_000_000));
        let int = DataType::Int(5);
        let big_int = DataType::BigInt(5);
        assert_eq!(format!("{}", tiny_text), "hi");
        assert_eq!(format!("{}", text), "this is a very long text indeed");
        assert_eq!(format!("{}", float_from_real), "-8.99");
        assert_eq!(format!("{}", float), "-8.99");
        assert_eq!(format!("{}", double_from_real), "-8.99");
        assert_eq!(format!("{}", double), "-8.99");
        assert_eq!(format!("{}", timestamp), "Thu Jan  1 00:00:00 1970");
        assert_eq!(format!("{}", int), "5");
        assert_eq!(format!("{}", big_int), "5");
    }

    fn _data_type_fungibility_test_eq<T>(f: &dyn for<'a> Fn(&'a DataType) -> T)
    where
        T: PartialEq + fmt::Debug,
    {
        let txt1: DataType = "hi".try_into().unwrap();
        let txt12: DataType = "no".try_into().unwrap();
        let txt2: DataType = DataType::Text(ArcCStr::try_from("hi").unwrap());
        let text: DataType = "this is a very long text indeed".try_into().unwrap();
        let text2: DataType = "this is another long text".try_into().unwrap();
        let float = DataType::Float(-8.99, 3);
        let float2 = DataType::Float(-8.98, 3);
        let float_from_real: DataType = DataType::try_from(-8.99_f32).unwrap();
        let float_from_real2: DataType = DataType::try_from(-8.98_f32).unwrap();
        let double = DataType::Double(-8.99, 3);
        let double2 = DataType::Double(-8.98, 3);
        let double_from_real: DataType = DataType::try_from(-8.99_f64).unwrap();
        let double_from_real2: DataType = DataType::try_from(-8.98_f64).unwrap();
        let time = DataType::Timestamp(NaiveDateTime::from_timestamp(0, 42_000_000));
        let time2 = DataType::Timestamp(NaiveDateTime::from_timestamp(1, 42_000_000));
        let shrt = DataType::Int(5);
        let shrt6 = DataType::Int(6);
        let long = DataType::BigInt(5);
        let long6 = DataType::BigInt(6);
        let ushrt = DataType::UnsignedInt(5);
        let ushrt6 = DataType::UnsignedInt(6);
        let ulong = DataType::UnsignedBigInt(5);
        let ulong6 = DataType::UnsignedBigInt(6);

        assert_eq!(f(&txt1), f(&txt1));
        assert_eq!(f(&txt2), f(&txt2));
        assert_eq!(f(&text), f(&text));
        assert_eq!(f(&shrt), f(&shrt));
        assert_eq!(f(&long), f(&long));
        assert_eq!(f(&ushrt), f(&ushrt));
        assert_eq!(f(&ulong), f(&ulong));
        assert_eq!(f(&float), f(&float));
        assert_eq!(f(&float_from_real), f(&float_from_real));
        assert_eq!(f(&double), f(&double));
        assert_eq!(f(&double_from_real), f(&double_from_real));
        assert_eq!(f(&time), f(&time));

        // coercion
        assert_eq!(f(&txt1), f(&txt2));
        assert_eq!(f(&txt2), f(&txt1));
        assert_eq!(f(&shrt), f(&long));
        assert_eq!(f(&long), f(&shrt));
        assert_eq!(f(&ushrt), f(&shrt));
        assert_eq!(f(&shrt), f(&ushrt));
        assert_eq!(f(&ulong), f(&long));
        assert_eq!(f(&long), f(&ulong));
        assert_eq!(f(&shrt), f(&ulong));
        assert_eq!(f(&ulong), f(&shrt));
        assert_eq!(f(&ushrt), f(&long));
        assert_eq!(f(&long), f(&ushrt));
        assert_eq!(f(&ushrt), f(&ulong));
        assert_eq!(f(&ulong), f(&ushrt));

        // negation
        assert_ne!(f(&txt1), f(&txt12));
        assert_ne!(f(&txt1), f(&text));
        assert_ne!(f(&txt1), f(&float));
        assert_ne!(f(&txt1), f(&float_from_real));
        assert_ne!(f(&txt1), f(&double));
        assert_ne!(f(&txt1), f(&double_from_real));
        assert_ne!(f(&txt1), f(&time));
        assert_ne!(f(&txt1), f(&shrt));
        assert_ne!(f(&txt1), f(&long));
        assert_ne!(f(&txt1), f(&ushrt));
        assert_ne!(f(&txt1), f(&ulong));

        assert_ne!(f(&txt2), f(&txt12));
        assert_ne!(f(&txt2), f(&text));
        assert_ne!(f(&txt2), f(&float));
        assert_ne!(f(&txt2), f(&float_from_real));
        assert_ne!(f(&txt2), f(&double));
        assert_ne!(f(&txt2), f(&double_from_real));
        assert_ne!(f(&txt2), f(&time));
        assert_ne!(f(&txt2), f(&shrt));
        assert_ne!(f(&txt2), f(&long));
        assert_ne!(f(&txt2), f(&ushrt));
        assert_ne!(f(&txt2), f(&ulong));

        assert_ne!(f(&text), f(&text2));
        assert_ne!(f(&text), f(&txt1));
        assert_ne!(f(&text), f(&txt2));
        assert_ne!(f(&text), f(&float));
        assert_ne!(f(&text), f(&float_from_real));
        assert_ne!(f(&text), f(&double));
        assert_ne!(f(&text), f(&double_from_real));
        assert_ne!(f(&text), f(&time));
        assert_ne!(f(&text), f(&shrt));
        assert_ne!(f(&text), f(&long));
        assert_ne!(f(&text), f(&ushrt));
        assert_ne!(f(&text), f(&ulong));

        assert_ne!(f(&float), f(&float2));
        assert_ne!(f(&float_from_real), f(&float_from_real2));
        assert_ne!(f(&double), f(&double2));
        assert_ne!(f(&double), f(&double_from_real2));
        assert_ne!(f(&double_from_real), f(&txt1));
        assert_ne!(f(&float), f(&txt1));
        assert_ne!(f(&float_from_real), f(&txt1));
        assert_ne!(f(&double), f(&txt1));
        assert_ne!(f(&double_from_real), f(&txt1));
        assert_ne!(f(&float), f(&txt2));
        assert_ne!(f(&float_from_real), f(&txt2));
        assert_ne!(f(&double), f(&txt2));
        assert_ne!(f(&double_from_real), f(&txt2));
        assert_ne!(f(&float), f(&text));
        assert_ne!(f(&float_from_real), f(&text));
        assert_ne!(f(&double), f(&text));
        assert_ne!(f(&double_from_real), f(&text));
        assert_ne!(f(&float), f(&time));
        assert_ne!(f(&float_from_real), f(&time));
        assert_ne!(f(&double), f(&time));
        assert_ne!(f(&double_from_real), f(&time));
        assert_ne!(f(&float), f(&shrt));
        assert_ne!(f(&float_from_real), f(&shrt));
        assert_ne!(f(&double), f(&shrt));
        assert_ne!(f(&double_from_real), f(&shrt));
        assert_ne!(f(&float), f(&long));
        assert_ne!(f(&float_from_real), f(&long));
        assert_ne!(f(&double), f(&long));
        assert_ne!(f(&double_from_real), f(&long));
        assert_ne!(f(&float), f(&ushrt));
        assert_ne!(f(&float_from_real), f(&ushrt));
        assert_ne!(f(&double), f(&ushrt));
        assert_ne!(f(&double_from_real), f(&ushrt));
        assert_ne!(f(&float), f(&ulong));
        assert_ne!(f(&float_from_real), f(&ulong));
        assert_ne!(f(&double), f(&ulong));
        assert_ne!(f(&double_from_real), f(&ulong));

        assert_ne!(f(&time), f(&time2));
        assert_ne!(f(&time), f(&txt1));
        assert_ne!(f(&time), f(&txt2));
        assert_ne!(f(&time), f(&text));
        assert_ne!(f(&time), f(&float_from_real));
        assert_ne!(f(&time), f(&shrt));
        assert_ne!(f(&time), f(&long));
        assert_ne!(f(&time), f(&ushrt));
        assert_ne!(f(&time), f(&ulong));

        assert_ne!(f(&shrt), f(&shrt6));
        assert_ne!(f(&shrt), f(&txt1));
        assert_ne!(f(&shrt), f(&txt2));
        assert_ne!(f(&shrt), f(&text));
        assert_ne!(f(&shrt), f(&float_from_real));
        assert_ne!(f(&shrt), f(&time));
        assert_ne!(f(&shrt), f(&long6));

        assert_ne!(f(&long), f(&long6));
        assert_ne!(f(&long), f(&txt1));
        assert_ne!(f(&long), f(&txt2));
        assert_ne!(f(&long), f(&text));
        assert_ne!(f(&long), f(&float_from_real));
        assert_ne!(f(&long), f(&time));
        assert_ne!(f(&long), f(&shrt6));

        assert_ne!(f(&ushrt), f(&ushrt6));
        assert_ne!(f(&ushrt), f(&txt1));
        assert_ne!(f(&ushrt), f(&txt2));
        assert_ne!(f(&ushrt), f(&text));
        assert_ne!(f(&ushrt), f(&float_from_real));
        assert_ne!(f(&ushrt), f(&time));
        assert_ne!(f(&ushrt), f(&ulong6));
        assert_ne!(f(&ushrt), f(&shrt6));
        assert_ne!(f(&ushrt), f(&long6));

        assert_ne!(f(&ulong), f(&ulong6));
        assert_ne!(f(&ulong), f(&txt1));
        assert_ne!(f(&ulong), f(&txt2));
        assert_ne!(f(&ulong), f(&text));
        assert_ne!(f(&ulong), f(&float_from_real));
        assert_ne!(f(&ulong), f(&time));
        assert_ne!(f(&ulong), f(&ushrt6));
        assert_ne!(f(&ulong), f(&shrt6));
        assert_ne!(f(&ulong), f(&long6));
    }

    #[test]
    fn data_type_conversion() {
        let int_i32_min = DataType::Int(std::i32::MIN);
        let int_u32_min = DataType::Int(std::u32::MIN as i32);
        let int_i32_max = DataType::Int(std::i32::MAX);
        let uint_u32_min = DataType::UnsignedInt(std::u32::MIN);
        let uint_i32_max = DataType::UnsignedInt(std::i32::MAX as u32);
        let uint_u32_max = DataType::UnsignedInt(std::u32::MAX);
        let bigint_i64_min = DataType::BigInt(std::i64::MIN);
        let bigint_i32_min = DataType::BigInt(std::i32::MIN as i64);
        let bigint_u32_min = DataType::BigInt(std::u32::MIN as i64);
        let bigint_i32_max = DataType::BigInt(std::i32::MAX as i64);
        let bigint_u32_max = DataType::BigInt(std::u32::MAX as i64);
        let bigint_i64_max = DataType::BigInt(std::i64::MAX);
        let ubigint_u32_min = DataType::UnsignedBigInt(std::u32::MIN as u64);
        let ubigint_i32_max = DataType::UnsignedBigInt(std::i32::MAX as u64);
        let ubigint_u32_max = DataType::UnsignedBigInt(std::u32::MAX as u64);
        let ubigint_i64_max = DataType::UnsignedBigInt(std::i64::MAX as u64);
        let ubigint_u64_max = DataType::UnsignedBigInt(std::u64::MAX);

        fn _data_type_conversion_test_eq_i32(d: &DataType) {
            assert_eq!(
                i32::try_from(d).unwrap() as i128,
                i128::try_from(d).unwrap()
            )
        }
        fn _data_type_conversion_test_eq_i32_panic(d: &DataType) {
            assert!(std::panic::catch_unwind(|| i32::try_from(d).unwrap()).is_err())
        }
        fn _data_type_conversion_test_eq_i64(d: &DataType) {
            assert_eq!(
                i64::try_from(d).unwrap() as i128,
                i128::try_from(d).unwrap()
            )
        }
        fn _data_type_conversion_test_eq_i64_panic(d: &DataType) {
            assert!(std::panic::catch_unwind(|| i64::try_from(d).unwrap()).is_err())
        }
        fn _data_type_conversion_test_eq_u32(d: &DataType) {
            assert_eq!(
                u32::try_from(d).unwrap() as i128,
                i128::try_from(d).unwrap()
            )
        }
        fn _data_type_conversion_test_eq_u32_panic(d: &DataType) {
            assert!(std::panic::catch_unwind(|| u32::try_from(d).unwrap()).is_err())
        }
        fn _data_type_conversion_test_eq_u64(d: &DataType) {
            assert_eq!(
                u64::try_from(d).unwrap() as i128,
                i128::try_from(d).unwrap()
            )
        }
        fn _data_type_conversion_test_eq_u64_panic(d: &DataType) {
            assert!(std::panic::catch_unwind(|| u64::try_from(d).unwrap()).is_err())
        }
        fn _data_type_conversion_test_eq_i128(d: &DataType) {
            assert_eq!(
                i128::try_from(d).unwrap() as i128,
                i128::try_from(d).unwrap()
            )
        }
        fn _data_type_conversion_test_eq_i128_panic(d: &DataType) {
            assert!(std::panic::catch_unwind(|| i128::try_from(d).unwrap()).is_err())
        }

        _data_type_conversion_test_eq_i32_panic(&bigint_i64_min);
        _data_type_conversion_test_eq_u32_panic(&bigint_i64_min);
        _data_type_conversion_test_eq_u64_panic(&bigint_i64_min);
        _data_type_conversion_test_eq_i64(&bigint_i64_min);
        _data_type_conversion_test_eq_i128(&bigint_i64_min);

        _data_type_conversion_test_eq_u32_panic(&int_i32_min);
        _data_type_conversion_test_eq_u64_panic(&int_i32_min);
        _data_type_conversion_test_eq_i32(&int_i32_min);
        _data_type_conversion_test_eq_i64(&int_i32_min);
        _data_type_conversion_test_eq_i128(&int_i32_min);
        _data_type_conversion_test_eq_u32_panic(&bigint_i32_min);
        _data_type_conversion_test_eq_u64_panic(&bigint_i32_min);
        _data_type_conversion_test_eq_i32(&bigint_i32_min);
        _data_type_conversion_test_eq_i64(&bigint_i32_min);
        _data_type_conversion_test_eq_i128(&bigint_i32_min);

        _data_type_conversion_test_eq_i32(&int_u32_min);
        _data_type_conversion_test_eq_i64(&int_u32_min);
        _data_type_conversion_test_eq_u32(&int_u32_min);
        _data_type_conversion_test_eq_u64(&int_u32_min);
        _data_type_conversion_test_eq_i128(&int_u32_min);
        _data_type_conversion_test_eq_i32(&uint_u32_min);
        _data_type_conversion_test_eq_i64(&uint_u32_min);
        _data_type_conversion_test_eq_u32(&uint_u32_min);
        _data_type_conversion_test_eq_u64(&uint_u32_min);
        _data_type_conversion_test_eq_i128(&uint_u32_min);
        _data_type_conversion_test_eq_i32(&bigint_u32_min);
        _data_type_conversion_test_eq_i64(&bigint_u32_min);
        _data_type_conversion_test_eq_u32(&bigint_u32_min);
        _data_type_conversion_test_eq_u64(&bigint_u32_min);
        _data_type_conversion_test_eq_i128(&bigint_u32_min);
        _data_type_conversion_test_eq_i32(&ubigint_u32_min);
        _data_type_conversion_test_eq_i64(&ubigint_u32_min);
        _data_type_conversion_test_eq_u32(&ubigint_u32_min);
        _data_type_conversion_test_eq_u64(&ubigint_u32_min);
        _data_type_conversion_test_eq_i128(&ubigint_u32_min);

        _data_type_conversion_test_eq_i32(&int_i32_max);
        _data_type_conversion_test_eq_i64(&int_i32_max);
        _data_type_conversion_test_eq_u32(&int_i32_max);
        _data_type_conversion_test_eq_u64(&int_i32_max);
        _data_type_conversion_test_eq_i128(&int_i32_max);
        _data_type_conversion_test_eq_i32(&uint_i32_max);
        _data_type_conversion_test_eq_i64(&uint_i32_max);
        _data_type_conversion_test_eq_u32(&uint_i32_max);
        _data_type_conversion_test_eq_u64(&uint_i32_max);
        _data_type_conversion_test_eq_i128(&uint_i32_max);
        _data_type_conversion_test_eq_i32(&bigint_i32_max);
        _data_type_conversion_test_eq_i64(&bigint_i32_max);
        _data_type_conversion_test_eq_u32(&bigint_i32_max);
        _data_type_conversion_test_eq_u64(&bigint_i32_max);
        _data_type_conversion_test_eq_i128(&bigint_i32_max);
        _data_type_conversion_test_eq_i32(&ubigint_i32_max);
        _data_type_conversion_test_eq_i64(&ubigint_i32_max);
        _data_type_conversion_test_eq_u32(&ubigint_i32_max);
        _data_type_conversion_test_eq_u64(&ubigint_i32_max);
        _data_type_conversion_test_eq_i128(&ubigint_i32_max);

        _data_type_conversion_test_eq_i32_panic(&uint_u32_max);
        _data_type_conversion_test_eq_i64(&uint_u32_max);
        _data_type_conversion_test_eq_u32(&uint_u32_max);
        _data_type_conversion_test_eq_u64(&uint_u32_max);
        _data_type_conversion_test_eq_i128(&uint_u32_max);
        _data_type_conversion_test_eq_i32_panic(&bigint_u32_max);
        _data_type_conversion_test_eq_i64(&bigint_u32_max);
        _data_type_conversion_test_eq_u32(&bigint_u32_max);
        _data_type_conversion_test_eq_u64(&bigint_u32_max);
        _data_type_conversion_test_eq_i128(&bigint_u32_max);
        _data_type_conversion_test_eq_i32_panic(&ubigint_u32_max);
        _data_type_conversion_test_eq_i64(&ubigint_u32_max);
        _data_type_conversion_test_eq_u32(&ubigint_u32_max);
        _data_type_conversion_test_eq_u64(&ubigint_u32_max);
        _data_type_conversion_test_eq_i128(&ubigint_u32_max);

        _data_type_conversion_test_eq_i32_panic(&bigint_i64_max);
        _data_type_conversion_test_eq_u32_panic(&bigint_i64_max);
        _data_type_conversion_test_eq_i64(&bigint_i64_max);
        _data_type_conversion_test_eq_u64(&bigint_i64_max);
        _data_type_conversion_test_eq_i128(&bigint_i64_max);
        _data_type_conversion_test_eq_i32_panic(&ubigint_i64_max);
        _data_type_conversion_test_eq_u32_panic(&ubigint_i64_max);
        _data_type_conversion_test_eq_i64(&ubigint_i64_max);
        _data_type_conversion_test_eq_u64(&ubigint_i64_max);
        _data_type_conversion_test_eq_i128(&ubigint_i64_max);

        _data_type_conversion_test_eq_i32_panic(&ubigint_u64_max);
        _data_type_conversion_test_eq_u32_panic(&ubigint_u64_max);
        _data_type_conversion_test_eq_i64_panic(&ubigint_u64_max);
        _data_type_conversion_test_eq_u64(&ubigint_u64_max);
        _data_type_conversion_test_eq_i128(&ubigint_u64_max);
    }

    #[proptest]
    fn data_type_string_conversion_roundtrip(s: String) {
        assert_eq!(
            String::try_from(&DataType::try_from(s.clone()).unwrap()).unwrap(),
            s
        )
    }

    #[test]
    #[allow(clippy::cognitive_complexity)]
    fn data_type_fungibility() {
        let hash = |dt: &DataType| {
            use std::collections::hash_map::DefaultHasher;
            use std::hash::{Hash, Hasher};
            let mut s = DefaultHasher::new();
            dt.hash(&mut s);
            s.finish()
        };
        let json_serialize = |dt: &DataType| -> DataType {
            serde_json::from_str(&serde_json::to_string(dt).unwrap()).unwrap()
        };
        let bincode_serialize = |dt: &DataType| -> DataType {
            bincode::deserialize(&bincode::serialize(dt).unwrap()).unwrap()
        };
        _data_type_fungibility_test_eq(&|x: &DataType| x.clone());
        _data_type_fungibility_test_eq(&hash);
        _data_type_fungibility_test_eq(&json_serialize);
        _data_type_fungibility_test_eq(&bincode_serialize);

        use std::convert::TryFrom;

        let txt1: DataType = "hi".try_into().unwrap();
        let txt12: DataType = "no".try_into().unwrap();
        let txt2: DataType = DataType::Text(ArcCStr::try_from("hi").unwrap());
        let text: DataType = "this is a very long text indeed".try_into().unwrap();
        let text2: DataType = "this is another long text".try_into().unwrap();
        let float = DataType::Float(-8.99, 3);
        let float2 = DataType::Float(-8.98, 3);
        let float_from_real: DataType = DataType::try_from(-8.99_f32).unwrap();
        let float_from_real2: DataType = DataType::try_from(-8.98_f32).unwrap();
        let double = DataType::Double(-8.99, 3);
        let double2 = DataType::Double(-8.98, 3);
        let double_from_real: DataType = DataType::try_from(-8.99_f64).unwrap();
        let double_from_real2: DataType = DataType::try_from(-8.98_f64).unwrap();
        let time = DataType::Timestamp(NaiveDateTime::from_timestamp(0, 42_000_000));
        let time2 = DataType::Timestamp(NaiveDateTime::from_timestamp(1, 42_000_000));
        let shrt = DataType::Int(5);
        let shrt6 = DataType::Int(6);
        let long = DataType::BigInt(5);
        let long6 = DataType::BigInt(6);
        let ushrt = DataType::UnsignedInt(5);
        let ushrt6 = DataType::UnsignedInt(6);
        let ulong = DataType::UnsignedBigInt(5);
        let ulong6 = DataType::UnsignedBigInt(6);

        use std::cmp::Ordering;
        assert_eq!(txt1.cmp(&txt1), Ordering::Equal);
        assert_eq!(txt2.cmp(&txt2), Ordering::Equal);
        assert_eq!(text.cmp(&text), Ordering::Equal);
        assert_eq!(shrt.cmp(&shrt), Ordering::Equal);
        assert_eq!(ushrt.cmp(&ushrt), Ordering::Equal);
        assert_eq!(long.cmp(&long), Ordering::Equal);
        assert_eq!(ulong.cmp(&ulong), Ordering::Equal);
        assert_eq!(float.cmp(&float), Ordering::Equal);
        assert_eq!(float_from_real.cmp(&float_from_real), Ordering::Equal);
        assert_eq!(double.cmp(&double), Ordering::Equal);
        assert_eq!(double_from_real.cmp(&double_from_real), Ordering::Equal);
        assert_eq!(time.cmp(&time), Ordering::Equal);

        // coercion
        assert_eq!(txt1.cmp(&txt2), Ordering::Equal);
        assert_eq!(txt2.cmp(&txt1), Ordering::Equal);
        assert_eq!(shrt.cmp(&long), Ordering::Equal);
        assert_eq!(shrt.cmp(&ulong), Ordering::Equal);
        assert_eq!(ushrt.cmp(&long), Ordering::Equal);
        assert_eq!(ushrt.cmp(&ulong), Ordering::Equal);
        assert_eq!(long.cmp(&shrt), Ordering::Equal);
        assert_eq!(long.cmp(&ushrt), Ordering::Equal);
        assert_eq!(ulong.cmp(&shrt), Ordering::Equal);
        assert_eq!(ulong.cmp(&ushrt), Ordering::Equal);

        // negation
        assert_ne!(txt1.cmp(&txt12), Ordering::Equal);
        assert_ne!(txt1.cmp(&text), Ordering::Equal);
        assert_ne!(txt1.cmp(&float), Ordering::Equal);
        assert_ne!(txt1.cmp(&float_from_real), Ordering::Equal);
        assert_ne!(txt1.cmp(&double), Ordering::Equal);
        assert_ne!(txt1.cmp(&double_from_real), Ordering::Equal);
        assert_ne!(txt1.cmp(&time), Ordering::Equal);
        assert_ne!(txt1.cmp(&shrt), Ordering::Equal);
        assert_ne!(txt1.cmp(&ushrt), Ordering::Equal);
        assert_ne!(txt1.cmp(&long), Ordering::Equal);
        assert_ne!(txt1.cmp(&ulong), Ordering::Equal);

        assert_ne!(txt2.cmp(&txt12), Ordering::Equal);
        assert_ne!(txt2.cmp(&text), Ordering::Equal);
        assert_ne!(txt2.cmp(&float), Ordering::Equal);
        assert_ne!(txt2.cmp(&float_from_real), Ordering::Equal);
        assert_ne!(txt2.cmp(&double), Ordering::Equal);
        assert_ne!(txt2.cmp(&double_from_real), Ordering::Equal);
        assert_ne!(txt2.cmp(&time), Ordering::Equal);
        assert_ne!(txt2.cmp(&shrt), Ordering::Equal);
        assert_ne!(txt2.cmp(&ushrt), Ordering::Equal);
        assert_ne!(txt2.cmp(&long), Ordering::Equal);
        assert_ne!(txt2.cmp(&ulong), Ordering::Equal);

        assert_ne!(text.cmp(&text2), Ordering::Equal);
        assert_ne!(text.cmp(&txt1), Ordering::Equal);
        assert_ne!(text.cmp(&txt2), Ordering::Equal);
        assert_ne!(text.cmp(&float), Ordering::Equal);
        assert_ne!(text.cmp(&float_from_real), Ordering::Equal);
        assert_ne!(text.cmp(&double), Ordering::Equal);
        assert_ne!(text.cmp(&double_from_real), Ordering::Equal);
        assert_ne!(text.cmp(&time), Ordering::Equal);
        assert_ne!(text.cmp(&shrt), Ordering::Equal);
        assert_ne!(text.cmp(&ushrt), Ordering::Equal);
        assert_ne!(text.cmp(&long), Ordering::Equal);
        assert_ne!(text.cmp(&ulong), Ordering::Equal);

        assert_ne!(float.cmp(&float2), Ordering::Equal);
        assert_ne!(float_from_real.cmp(&float_from_real2), Ordering::Equal);
        assert_ne!(double.cmp(&double2), Ordering::Equal);
        assert_ne!(double_from_real.cmp(&double_from_real2), Ordering::Equal);
        assert_ne!(float.cmp(&txt1), Ordering::Equal);
        assert_ne!(float_from_real.cmp(&txt1), Ordering::Equal);
        assert_ne!(double.cmp(&txt1), Ordering::Equal);
        assert_ne!(double_from_real.cmp(&txt1), Ordering::Equal);
        assert_ne!(float.cmp(&txt2), Ordering::Equal);
        assert_ne!(float_from_real.cmp(&txt2), Ordering::Equal);
        assert_ne!(double.cmp(&txt2), Ordering::Equal);
        assert_ne!(double_from_real.cmp(&txt2), Ordering::Equal);
        assert_ne!(float.cmp(&text), Ordering::Equal);
        assert_ne!(float_from_real.cmp(&text), Ordering::Equal);
        assert_ne!(double.cmp(&text), Ordering::Equal);
        assert_ne!(double_from_real.cmp(&text), Ordering::Equal);
        assert_ne!(float.cmp(&time), Ordering::Equal);
        assert_ne!(float_from_real.cmp(&time), Ordering::Equal);
        assert_ne!(double.cmp(&time), Ordering::Equal);
        assert_ne!(double_from_real.cmp(&time), Ordering::Equal);
        assert_ne!(float.cmp(&shrt), Ordering::Equal);
        assert_ne!(float_from_real.cmp(&shrt), Ordering::Equal);
        assert_ne!(double.cmp(&shrt), Ordering::Equal);
        assert_ne!(double_from_real.cmp(&shrt), Ordering::Equal);
        assert_ne!(float.cmp(&shrt), Ordering::Equal);
        assert_ne!(float_from_real.cmp(&shrt), Ordering::Equal);
        assert_ne!(double.cmp(&shrt), Ordering::Equal);
        assert_ne!(double_from_real.cmp(&shrt), Ordering::Equal);
        assert_ne!(float.cmp(&ushrt), Ordering::Equal);
        assert_ne!(float_from_real.cmp(&ushrt), Ordering::Equal);
        assert_ne!(double.cmp(&ushrt), Ordering::Equal);
        assert_ne!(double_from_real.cmp(&ushrt), Ordering::Equal);
        assert_ne!(float.cmp(&long), Ordering::Equal);
        assert_ne!(float_from_real.cmp(&long), Ordering::Equal);
        assert_ne!(double.cmp(&long), Ordering::Equal);
        assert_ne!(double_from_real.cmp(&long), Ordering::Equal);
        assert_ne!(float.cmp(&ulong), Ordering::Equal);
        assert_ne!(float_from_real.cmp(&ulong), Ordering::Equal);
        assert_ne!(double.cmp(&ulong), Ordering::Equal);
        assert_ne!(double_from_real.cmp(&ulong), Ordering::Equal);

        assert_ne!(time.cmp(&time2), Ordering::Equal);
        assert_ne!(time.cmp(&txt1), Ordering::Equal);
        assert_ne!(time.cmp(&txt2), Ordering::Equal);
        assert_ne!(time.cmp(&text), Ordering::Equal);
        assert_ne!(time.cmp(&float), Ordering::Equal);
        assert_ne!(time.cmp(&float_from_real), Ordering::Equal);
        assert_ne!(time.cmp(&double), Ordering::Equal);
        assert_ne!(time.cmp(&double_from_real), Ordering::Equal);
        assert_ne!(time.cmp(&shrt), Ordering::Equal);
        assert_ne!(time.cmp(&ushrt), Ordering::Equal);
        assert_ne!(time.cmp(&long), Ordering::Equal);
        assert_ne!(time.cmp(&ulong), Ordering::Equal);

        assert_ne!(shrt.cmp(&shrt6), Ordering::Equal);
        assert_ne!(shrt.cmp(&ushrt6), Ordering::Equal);
        assert_ne!(shrt.cmp(&txt1), Ordering::Equal);
        assert_ne!(shrt.cmp(&txt2), Ordering::Equal);
        assert_ne!(shrt.cmp(&text), Ordering::Equal);
        assert_ne!(shrt.cmp(&float), Ordering::Equal);
        assert_ne!(shrt.cmp(&float_from_real), Ordering::Equal);
        assert_ne!(shrt.cmp(&double), Ordering::Equal);
        assert_ne!(shrt.cmp(&double_from_real), Ordering::Equal);
        assert_ne!(shrt.cmp(&time), Ordering::Equal);
        assert_ne!(shrt.cmp(&long6), Ordering::Equal);
        assert_ne!(shrt.cmp(&ulong6), Ordering::Equal);

        assert_ne!(ushrt.cmp(&shrt6), Ordering::Equal);
        assert_ne!(ushrt.cmp(&ushrt6), Ordering::Equal);
        assert_ne!(ushrt.cmp(&txt1), Ordering::Equal);
        assert_ne!(ushrt.cmp(&txt2), Ordering::Equal);
        assert_ne!(ushrt.cmp(&text), Ordering::Equal);
        assert_ne!(ushrt.cmp(&float), Ordering::Equal);
        assert_ne!(ushrt.cmp(&float_from_real), Ordering::Equal);
        assert_ne!(ushrt.cmp(&double), Ordering::Equal);
        assert_ne!(ushrt.cmp(&double_from_real), Ordering::Equal);
        assert_ne!(ushrt.cmp(&time), Ordering::Equal);
        assert_ne!(ushrt.cmp(&long6), Ordering::Equal);
        assert_ne!(ushrt.cmp(&ulong6), Ordering::Equal);

        assert_ne!(long.cmp(&long6), Ordering::Equal);
        assert_ne!(long.cmp(&ulong6), Ordering::Equal);
        assert_ne!(long.cmp(&txt1), Ordering::Equal);
        assert_ne!(long.cmp(&txt2), Ordering::Equal);
        assert_ne!(long.cmp(&text), Ordering::Equal);
        assert_ne!(long.cmp(&float), Ordering::Equal);
        assert_ne!(long.cmp(&float_from_real), Ordering::Equal);
        assert_ne!(long.cmp(&double), Ordering::Equal);
        assert_ne!(long.cmp(&double_from_real), Ordering::Equal);
        assert_ne!(long.cmp(&time), Ordering::Equal);
        assert_ne!(long.cmp(&shrt6), Ordering::Equal);
        assert_ne!(long.cmp(&ushrt6), Ordering::Equal);

        assert_ne!(ulong.cmp(&long6), Ordering::Equal);
        assert_ne!(ulong.cmp(&ulong6), Ordering::Equal);
        assert_ne!(ulong.cmp(&txt1), Ordering::Equal);
        assert_ne!(ulong.cmp(&txt2), Ordering::Equal);
        assert_ne!(ulong.cmp(&text), Ordering::Equal);
        assert_ne!(ulong.cmp(&float), Ordering::Equal);
        assert_ne!(ulong.cmp(&float_from_real), Ordering::Equal);
        assert_ne!(ulong.cmp(&double), Ordering::Equal);
        assert_ne!(ulong.cmp(&double_from_real), Ordering::Equal);
        assert_ne!(ulong.cmp(&time), Ordering::Equal);
        assert_ne!(ulong.cmp(&shrt6), Ordering::Equal);
        assert_ne!(ulong.cmp(&ushrt6), Ordering::Equal);

        // Test invariants
        // 1. Text types always > everythign else
        // 2. Double & float comparable directly with int
        let int1 = DataType::Int(1);
        let float1 = DataType::Float(1.2, 16);
        let float2 = DataType::Float(0.8, 16);
        let double1 = DataType::Double(1.2, 16);
        let double2 = DataType::Double(0.8, 16);
        assert_eq!(txt1.cmp(&int1), Ordering::Greater);
        assert_eq!(int1.cmp(&txt1), Ordering::Less);
        assert_eq!(txt1.cmp(&double1), Ordering::Greater);
        assert_eq!(float1.cmp(&txt1), Ordering::Less);
        assert_eq!(float1.cmp(&int1), Ordering::Greater);
        assert_eq!(float2.cmp(&int1), Ordering::Less);
        assert_eq!(double1.cmp(&txt1), Ordering::Less);
        assert_eq!(double1.cmp(&int1), Ordering::Greater);
        assert_eq!(double2.cmp(&int1), Ordering::Less);
    }

    #[allow(clippy::eq_op)]
    mod eq {
        use super::*;
        use launchpad::hash::hash;
        use test_strategy::proptest;

        #[proptest]
        fn reflexive(dt: DataType) {
            assert!(dt == dt);
        }

        #[proptest]
        fn symmetric(dt1: DataType, dt2: DataType) {
            assert_eq!(dt1 == dt2, dt2 == dt1);
        }

        #[proptest]
        fn transitive(dt1: DataType, dt2: DataType, dt3: DataType) {
            if dt1 == dt2 && dt2 == dt3 {
                assert!(dt1 == dt3)
            }
        }

        #[proptest]
        fn matches_hash(dt1: DataType, dt2: DataType) {
            assert_eq!(dt1 == dt2, hash(&dt1) == hash(&dt2));
        }
    }

    mod coerce_to {
        use super::*;
        use launchpad::arbitrary::{
            arbitrary_naive_date, arbitrary_naive_date_time, arbitrary_naive_time,
        };
        use proptest::sample::select;
        use proptest::strategy::Strategy;
        use test_strategy::proptest;
        use SqlType::*;

        #[proptest]
        fn same_type_is_identity(dt: DataType) {
            if let Some(ty) = dt.sql_type() {
                assert_eq!(dt.coerce_to(&ty).as_deref().unwrap(), &dt);
            }
        }

        #[proptest]
        fn parse_timestamps(#[strategy(arbitrary_naive_date_time())] ndt: NaiveDateTime) {
            let expected = DataType::from(ndt);
            let input = DataType::try_from(ndt.format(TIMESTAMP_FORMAT).to_string()).unwrap();
            let result = input.coerce_to(&Timestamp).unwrap();
            assert_eq!(*result, expected);
        }

        #[proptest]
        fn parse_times(#[strategy(arbitrary_naive_time())] nt: NaiveTime) {
            let expected = DataType::from(nt);
            let input = DataType::try_from(nt.format(TIME_FORMAT).to_string()).unwrap();
            let result = input.coerce_to(&Time).unwrap();
            assert_eq!(*result, expected);
        }

        #[proptest]
        fn parse_datetimes(#[strategy(arbitrary_naive_date())] nd: NaiveDate) {
            let dt = NaiveDateTime::new(nd, NaiveTime::from_hms(12, 0, 0));
            let expected = DataType::from(dt);
            let input = DataType::try_from(dt.format(TIMESTAMP_FORMAT).to_string()).unwrap();
            let result = input.coerce_to(&SqlType::DateTime(None)).unwrap();
            assert_eq!(*result, expected);
        }

        #[proptest]
        fn parse_dates(#[strategy(arbitrary_naive_date())] nd: NaiveDate) {
            let expected = DataType::from(NaiveDateTime::new(nd, NaiveTime::from_hms(0, 0, 0)));
            let input = DataType::try_from(nd.format(DATE_FORMAT).to_string()).unwrap();
            let result = input.coerce_to(&Date).unwrap();
            assert_eq!(*result, expected);
        }

        #[test]
        fn timestamp_surjections() {
            let input = DataType::from(NaiveDate::from_ymd(2021, 3, 17).and_hms(11, 34, 56));
            assert_eq!(
                *input.coerce_to(&Date).unwrap(),
                NaiveDate::from_ymd(2021, 3, 17).into()
            );
            assert_eq!(
                *input.coerce_to(&Time).unwrap(),
                NaiveTime::from_hms(11, 34, 56).into()
            );
        }

        macro_rules! int_conversion {
            ($name: ident, $from: ty, $to: ty, $sql_type: expr) => {
                #[proptest]
                fn $name(source: $to) {
                    let input = <$from>::try_from(source);
                    prop_assume!(input.is_ok());
                    assert_eq!(
                        *DataType::from(input.unwrap())
                            .coerce_to(&$sql_type)
                            .unwrap(),
                        DataType::from(source)
                    );
                }
            };
        }

        int_conversion!(int_to_tinyint, i32, i8, Tinyint(None));
        int_conversion!(int_to_unsigned_tinyint, i32, u8, UnsignedTinyint(None));
        int_conversion!(int_to_smallint, i32, i16, Smallint(None));
        int_conversion!(int_to_unsigned_smallint, i32, u16, UnsignedSmallint(None));
        int_conversion!(bigint_to_tinyint, i64, i8, Tinyint(None));
        int_conversion!(bigint_to_unsigned_tinyint, i64, u8, UnsignedTinyint(None));
        int_conversion!(bigint_to_smallint, i64, i16, Smallint(None));
        int_conversion!(
            bigint_to_unsigned_smallint,
            i64,
            u16,
            UnsignedSmallint(None)
        );
        int_conversion!(bigint_to_int, i64, i32, Int(None));
        int_conversion!(bigint_to_unsigned_bigint, i64, u64, UnsignedBigint(None));

        macro_rules! real_conversion {
            ($name: ident, $from: ty, $to: ty, $sql_type: expr) => {
                #[proptest]
                fn $name(source: $from) {
                    if (source as $to).is_finite() {
                        assert_eq!(
                            *DataType::try_from(source)
                                .unwrap()
                                .coerce_to(&$sql_type)
                                .unwrap(),
                            DataType::try_from(source as $to).unwrap()
                        );
                    } else {
                        assert!(DataType::try_from(source)
                            .unwrap()
                            .coerce_to(&$sql_type)
                            .is_err());
                        assert!(DataType::try_from(source as $to).is_err());
                    }
                }
            };
        }

        real_conversion!(float_to_double, f32, f64, Double);
        real_conversion!(double_to_float, f64, f32, Float);

        fn int_type() -> impl Strategy<Value = SqlType> {
            use SqlType::*;
            select(vec![Tinyint(None), Smallint(None), Int(None), Bigint(None)])
        }

        #[proptest]
        fn double_to_int(whole_part: i32, #[strategy(int_type())] int_type: SqlType) {
            let double = DataType::Double(whole_part as f64, 0);
            let result = double.coerce_to(&int_type).unwrap();
            assert_eq!(i32::try_from(result.into_owned()).unwrap(), whole_part);
        }

        fn unsigned_type() -> impl Strategy<Value = SqlType> {
            use SqlType::*;
            select(vec![
                UnsignedTinyint(None),
                UnsignedSmallint(None),
                UnsignedInt(None),
                UnsignedBigint(None),
            ])
        }

        #[proptest]
        fn double_to_unsigned(
            whole_part: u32,
            #[strategy(unsigned_type())] unsigned_type: SqlType,
        ) {
            let double = DataType::Double(whole_part as f64, 0);
            let result = double.coerce_to(&unsigned_type).unwrap();
            assert_eq!(u32::try_from(result.into_owned()).unwrap(), whole_part);
        }

        #[proptest]
        fn char_equal_length(#[strategy("a{1,30}")] text: String) {
            use SqlType::*;
            let input = DataType::try_from(text.as_str()).unwrap();
            let intermediate = Char(Some(u16::try_from(text.len()).unwrap()));
            let result = input.coerce_to(&intermediate).unwrap();
            assert_eq!(
                String::try_from(&result.into_owned()).unwrap().as_str(),
                text.as_str()
            );
        }

        #[test]
        fn text_to_json() {
            let input = DataType::from("{\"foo\": \"bar\"}");
            let result = input.coerce_to(&SqlType::Json).unwrap();
            assert_eq!(&input, result.as_ref());
        }

        macro_rules! bool_conversion {
            ($name: ident, $ty: ty) => {
                #[proptest]
                fn $name(input: $ty) {
                    let input_dt = DataType::from(input);
                    let result = input_dt.coerce_to(&SqlType::Bool).unwrap();
                    let expected = DataType::from(input != 0);
                    assert_eq!(result.as_ref(), &expected);
                }
            };
        }

        bool_conversion!(i8_to_bool, i8);
        bool_conversion!(u8_to_bool, u8);
        bool_conversion!(i16_to_bool, i16);
        bool_conversion!(u16_to_bool, u16);
        bool_conversion!(i32_to_bool, i32);
        bool_conversion!(u32_to_bool, u32);
        bool_conversion!(i64_to_bool, i64);
        bool_conversion!(u64_to_bool, u64);
    }
}
